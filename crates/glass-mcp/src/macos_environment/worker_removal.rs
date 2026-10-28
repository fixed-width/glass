//! Confirm logged-out Glass job absence without removing the account's Background domain.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
struct UserDomain {
    uid: u32,
    asid: u32,
    creator: String,
    gui_asid: Option<u32>,
    children: BTreeSet<String>,
    glass_jobs: bool,
}

impl UserDomain {
    fn require_logged_out(&self) -> anyhow::Result<()> {
        ensure!(
            self.gui_asid.is_none() && self.children.is_empty() && !self.glass_jobs,
            "account still has graphical or Glass jobs; registration retained"
        );
        Ok(())
    }
}

fn number(value: &str) -> anyhow::Result<u32> {
    ensure!(
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid launchd domain identity"
    );
    let value: u32 = value.parse()?;
    ensure!(
        value > 0 && value <= i32::MAX as u32,
        "invalid launchd domain identity"
    );
    Ok(value)
}

fn parse_user_domain(output: &str, uid: u32) -> anyhow::Result<UserDomain> {
    ensure!(
        output.len() <= 8 * 1024 * 1024,
        "domain listing exceeded its bound"
    );
    let lines: Vec<_> = output.lines().collect();
    ensure!(
        lines.first().copied() == Some(format!("user/{uid} = {{").as_str())
            && lines.last().copied() == Some("}"),
        "incomplete or mismatched user-domain listing"
    );
    let mut depth = 1usize;
    let mut section = String::new();
    let mut fields = BTreeMap::new();
    let mut security = BTreeMap::new();
    let mut children = BTreeSet::new();
    let mut glass_jobs = false;
    for (index, line) in lines.iter().enumerate().skip(1) {
        let text = line.trim();
        if text == "}" {
            depth = depth.checked_sub(1).context("unbalanced domain listing")?;
            ensure!(
                depth != 0 || index == lines.len() - 1,
                "premature domain ending"
            );
            continue;
        }
        ensure!(depth != 0, "data after domain ending");
        if depth == 1 && !text.is_empty() {
            let (key, value) = text.split_once(" =").context("invalid user-domain field")?;
            ensure!(
                fields
                    .insert(key.to_owned(), value.trim().to_owned())
                    .is_none(),
                "duplicate user-domain field"
            );
            section = key.to_owned();
        } else if depth == 2 {
            match section.as_str() {
                "security context" => {
                    let (key, value) = text
                        .split_once(" = ")
                        .context("invalid security-context field")?;
                    ensure!(
                        security
                            .insert(key.to_owned(), value.trim().to_owned())
                            .is_none(),
                        "duplicate security-context field"
                    );
                }
                "subdomains" if !text.is_empty() => {
                    let (kind, id) = text.split_once('/').context("invalid subdomain")?;
                    ensure!(
                        matches!(kind, "login" | "gui" | "pid" | "session"),
                        "unknown subdomain kind"
                    );
                    number(id)?;
                    ensure!(children.insert(text.to_owned()), "duplicate subdomain");
                }
                "services" if !text.is_empty() => {
                    let values: Vec<_> = text.split_whitespace().collect();
                    ensure!(
                        values.len() == 3
                            && values[0].parse::<u32>().is_ok()
                            && (values[1] == "-" || values[1].parse::<i32>().is_ok())
                            && values[2]
                                .bytes()
                                .all(|byte| byte.is_ascii_graphic() && !b"{}\"".contains(&byte)),
                        "unsupported service listing row"
                    );
                    glass_jobs |= matches!(values[2], LABEL | ONBOARD_LABEL);
                }
                _ => {}
            }
        }
        ensure!(
            depth <= 2
                || !matches!(
                    section.as_str(),
                    "services" | "security context" | "subdomains"
                ),
            "nested critical domain field"
        );
        if text.ends_with(" = {") || text.ends_with(" => {") {
            depth += 1;
            ensure!(depth <= 16, "excessive domain nesting");
        }
    }
    ensure!(depth == 0, "unclosed domain listing");
    let field = |key: &str| {
        fields
            .get(key)
            .map(String::as_str)
            .context("missing user-domain field")
    };
    ensure!(
        field("type")? == "user" && number(field("handle")?)? == uid,
        "wrong user-domain binding"
    );
    ensure!(
        field("session")? == "Background",
        "unexpected user-domain session"
    );
    ensure!(
        security.get("uid").map(String::as_str) == Some(uid.to_string().as_str()),
        "wrong security-context UID"
    );
    for key in ["security context", "services"] {
        ensure!(field(key)? == "{", "missing or unsupported {key} section");
    }
    ensure!(
        fields.get("subdomains").is_none_or(|value| value == "{"),
        "unsupported subdomain section"
    );
    let asid = number(security.get("asid").context("missing background ASID")?)?;
    let creator = field("creator")?.to_owned();
    let (program, pid) = creator
        .rsplit_once('[')
        .context("missing domain creator identity")?;
    ensure!(
        !program.is_empty()
            && program
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && !b"[]{}".contains(&byte)),
        "invalid domain creator"
    );
    number(
        pid.strip_suffix(']')
            .context("invalid domain creator PID")?,
    )?;
    ensure!(
        !creator
            .split('[')
            .next()
            .unwrap_or_default()
            .ends_with("launchctl"),
        "domain was materialized by an inspection; registration retained"
    );
    let gui_asid = fields
        .get("gui asid")
        .map(|value| number(value))
        .transpose()?;
    Ok(UserDomain {
        uid,
        asid,
        creator,
        gui_asid,
        children,
        glass_jobs,
    })
}

fn user_domain(uid: u32) -> anyhow::Result<UserDomain> {
    let listing = crate::account_uid::system_listing(
        Command::new("/bin/launchctl")
            .args(["print", &format!("user/{uid}")])
            .env("LC_ALL", "C"),
        Duration::from_secs(5),
    )?;
    parse_user_domain(&listing, uid)
}

fn no_glass_processes(account: &Enrollment, root: &File) -> anyhow::Result<()> {
    let requirement =
        stored_file(root, "worker.requirement")?.context("worker code requirement unavailable")?;
    let uid = account.uid;
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        // Late results carry only observations; this thread never mutates service state.
        let _ = send.send(inspect_processes(uid, &requirement));
    });
    receive
        .recv_timeout(Duration::from_secs(5))
        .context("Glass process inspection timed out; registration retained")?
}

fn inspect_processes(uid: u32, requirement: &[u8]) -> anyhow::Result<()> {
    inspect_processes_with(uid, requirement, || private_session::account_processes(uid))
}

fn inspect_processes_with(
    uid: u32,
    requirement: &[u8],
    mut processes: impl FnMut() -> std::io::Result<Vec<private_session::ProcessIdentity>>,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut previous = None;
    for _ in 0..3 {
        let mut current = processes()?;
        current.sort_unstable_by_key(private_session::ProcessIdentity::pid);
        for process in &current {
            ensure!(
                started.elapsed() < Duration::from_secs(5),
                "Glass process inspection exceeded its bound"
            );
            match private_session::process_matches_requirement(uid, process, requirement) {
                Ok(false) => {}
                Ok(true) => bail!(
                    "a Glass process still runs in the logged-out account; registration retained"
                ),
                Err(error) => {
                    if processes()?.contains(process) {
                        return Err(error)
                            .context("a live process's Glass identity could not be inspected");
                    }
                }
            }
        }
        if previous.as_ref() == Some(&current) {
            return Ok(());
        }
        previous = Some(current);
    }
    bail!("the account process set keeps changing; registration retained")
}

fn confirm_logged_out(account: &Enrollment, root: &File) -> anyhow::Result<()> {
    let before = crate::account_uid::existing_domain_targets()?;
    ensure!(
        before.contains(&("user".to_owned(), account.owner_uid)),
        "normal account domain unavailable"
    );
    let control = user_domain(account.owner_uid)?;
    let control_asid = control
        .gui_asid
        .context("normal-account graphical positive control failed")?;
    ensure!(
        private_session::session_exists(control_asid, account.owner_uid)?,
        "normal-account graphical positive control failed"
    );
    ensure!(
        !before.contains(&("gui".to_owned(), account.uid)),
        "graphical domain appeared in system listing"
    );
    ensure!(
        private_session::live_login_for(account.owner_uid, account.uid)?
            == private_session::LoginObservation::NotObserved,
        "development account has a live graphical login; registration retained"
    );
    let observed = if before.contains(&("user".to_owned(), account.uid)) {
        let domain = user_domain(account.uid)?;
        ensure!(
            private_session::session_exists(domain.asid, account.uid)?,
            "background domain audit identity could not be verified"
        );
        domain.require_logged_out()?;
        Some(domain)
    } else {
        None
    };
    no_glass_processes(account, root)?;
    let after = crate::account_uid::existing_domain_targets()?;
    ensure!(
        !after.contains(&("gui".to_owned(), account.uid)),
        "graphical domain appeared during cleanup inspection"
    );
    ensure!(
        private_session::live_login_for(account.owner_uid, account.uid)?
            == private_session::LoginObservation::NotObserved,
        "development login changed during cleanup inspection"
    );
    ensure!(
        before.contains(&("user".to_owned(), account.uid))
            == after.contains(&("user".to_owned(), account.uid)),
        "account domain changed during cleanup inspection"
    );
    if let Some(observed) = observed {
        ensure!(
            user_domain(account.uid)? == observed,
            "account graphical domain changed during cleanup inspection"
        );
    }
    ensure!(
        user_domain(account.owner_uid)?.gui_asid == Some(control_asid),
        "normal account login changed during cleanup inspection"
    );
    Ok(())
}

/// Strict launchd inspection, with independently verified logged-out absence only for removal.
pub(super) fn inspect(
    target: &str,
    account: &Enrollment,
    root: &File,
) -> anyhow::Result<Option<String>> {
    match crate::launchd::inspect_for_setup(target)? {
        crate::launchd::Inspection::Loaded(job) => Ok(Some(job)),
        crate::launchd::Inspection::MissingJob | crate::launchd::Inspection::MissingDomain => {
            Ok(None)
        }
        crate::launchd::Inspection::GuiUnavailable => {
            confirm_logged_out(account, root)
                .context("cannot confirm logged-out Glass job removal")?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn background() -> String {
        "user/508 = {\n\ttype = user\n\thandle = 508\n\tcreator = screensharingd[123]\n\tsession = Background\n\tsecurity context = {\n\t\tuid = 508\n\t\tasid = 101294\n\t}\n\tservices = {\n\t\t0 - com.apple.cfprefsd.xpc.agent\n\t}\n}\n".into()
    }

    #[test]
    fn background_and_graphical_association_are_distinct() {
        let empty = parse_user_domain(&background(), 508).unwrap();
        assert!(empty.gui_asid.is_none() && empty.children.is_empty() && !empty.glass_jobs);
        let graphical = background().replace(
            "\tsession = Background",
            "\tgui asid = 100002\n\tsession = Background",
        );
        assert_eq!(
            parse_user_domain(&graphical, 508).unwrap().gui_asid,
            Some(100002)
        );
        for label in [LABEL, ONBOARD_LABEL] {
            let job = background().replace("com.apple.cfprefsd.xpc.agent", label);
            assert!(parse_user_domain(&job, 508).unwrap().glass_jobs);
        }
    }

    #[test]
    fn incomplete_ambiguous_or_materialized_domain_cannot_authorize_cleanup() {
        for output in [
            background().replace("user/508", "user/509"),
            background().replace("\t\tuid = 508", "\t\tuid = 509"),
            background().replace("\thandle = 508", "\thandle = 509"),
            background().replace("\ttype = user", "\ttype = user\n\ttype = user"),
            background().replace("screensharingd[123]", "launchctl[456]"),
            background().replace("\t\tasid = 101294", "\t\tasid = nope"),
            background().trim_end_matches("}\n").to_owned(),
        ] {
            assert!(parse_user_domain(&output, 508).is_err(), "{output}");
        }
    }

    #[test]
    fn unsupported_sections_and_nested_spoofs_never_prove_absence() {
        let services = "\tservices = {\n\t\t0 - com.apple.cfprefsd.xpc.agent\n\t}\n";
        for output in [
            background().replace(services, ""),
            background().replace(services, "\tservices = unavailable\n"),
            background().replace(services, "\tservices = {}\n"),
            background().replace(services, "\tservices = {\n\t\tunexpected row\n\t}\n"),
            background().replace(services, "\tservices = {\n\t\tnested = {\n\t\t}\n\t}\n"),
            background().replace(services, "\tsubdomains = unavailable\n"),
            background().replace(
                "\tsecurity context = {",
                "\tsecurity context = broken\n\tignored = {",
            ),
            background().replace("\t\tuid = 508", "\t\tuid = 508\n\t\tuid = 508"),
            background().replace("screensharingd[123]", ""),
        ] {
            assert!(parse_user_domain(&output, 508).is_err(), "{output}");
        }
    }

    #[test]
    fn live_graphical_or_glass_jobs_prevent_removal() {
        for output in [
            background().replace(
                "\tsession = Background",
                "\tgui asid = 123\n\tsession = Background",
            ),
            background().replace("com.apple.cfprefsd.xpc.agent", LABEL),
            background().replace("com.apple.cfprefsd.xpc.agent", ONBOARD_LABEL),
            background().replace(
                "\tservices = {",
                "\tsubdomains = {\n\t\tlogin/123\n\t}\n\tservices = {",
            ),
        ] {
            assert!(
                parse_user_domain(&output, 508)
                    .unwrap()
                    .require_logged_out()
                    .is_err()
            );
        }
    }

    #[test]
    #[ignore = "requires administrator ownership for isolated temporary registration files"]
    fn temporary_unenrollment_removes_only_owned_files_and_enables_creation() {
        use glass_macos::setup_window::{SetupProgress, SetupStep};
        use std::os::unix::net::UnixListener;
        assert!(rustix::process::geteuid().is_root());
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("accounts");
        std::fs::create_dir(&parent).unwrap();
        let parent_fd = File::open(&parent).unwrap();
        for fail in [true, false] {
            let root_path = parent.join("501");
            let agents_path = temporary.path().join(if fail {
                "failed-agents"
            } else {
                "success-agents"
            });
            std::fs::create_dir(&root_path).unwrap();
            std::fs::create_dir(&agents_path).unwrap();
            let root = File::open(&root_path).unwrap();
            let agents = File::open(&agents_path).unwrap();
            let account = Enrollment {
                version: 1,
                owner_uid: 501,
                uid: 508,
                gid: 20,
                name: "temporary-test-account".into(),
                record_uuid: "test-record".into(),
                home: temporary.path().to_owned(),
            };
            let binary = Path::new("/Applications/GlassMcp.app/Contents/MacOS/glass-mcp");
            create_file(&root, ACCOUNT, b"temporary account binding").unwrap();
            create_file(&root, "worker.requirement", b"temporary code binding").unwrap();
            create_file(&agents, JOB, &job(&account, binary).unwrap()).unwrap();
            let helper = onboarding_job(&account, binary).unwrap();
            create_file(&root, ONBOARD_JOB, &helper).unwrap();
            create_file(&agents, ONBOARD_JOB, &helper).unwrap();
            let _socket = UnixListener::bind(root_path.join("worker.sock")).unwrap();
            std::fs::set_permissions(
                root_path.join("worker.sock"),
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            )
            .unwrap();
            let mut calls = 0;
            let mut progress = SetupProgress {
                permission_grants: None,
                creation_blocker: Some("enrolled account".into()),
                account: account.name.clone(),
                message: String::new(),
                step: SetupStep::Account,
                running: false,
                can_cancel: true,
                start_account: None,
                resume_creation: None,
            };
            let result = remove_registered_jobs_with(
                &root,
                &agents,
                &account,
                binary,
                false,
                |_, _, _| {
                    calls += 1;
                    if fail {
                        bail!("unconfirmed domain ownership");
                    }
                    Ok(None)
                },
                |_, _| panic!("absent jobs must never be stopped"),
            );
            if fail {
                assert!(result.is_err());
                assert!(root_path.join(ACCOUNT).exists());
                assert!(root_path.join("worker.sock").exists());
                assert!(agents_path.join(JOB).exists());
                assert!(!progress.can_create());
                std::fs::remove_dir_all(&root_path).unwrap();
            } else {
                result.unwrap();
                assert!(calls >= 5);
                assert!(!root_path.join("worker.sock").exists());
                assert!(!agents_path.join(JOB).exists());
                assert!(!agents_path.join(ONBOARD_JOB).exists());
                retire_registration(&parent_fd, "501", &root).unwrap();
                assert!(!root_path.exists());
                progress.isolation_disabled();
                assert!(progress.can_create());
            }
            assert!(temporary.path().exists());
        }
        println!(
            "isolated removal failure preserved registration; successful removal enabled Create account; no real enrollment or account changed"
        );
    }

    #[test]
    #[ignore = "read-only administrator check of a currently enrolled, logged-out account"]
    fn enrolled_logged_out_removal_preflight() {
        assert!(rustix::process::geteuid().is_root());
        let account = read_enrollment().unwrap();
        let root = directory(&owner_root().unwrap(), 0, false).unwrap();
        let account_before = stored_file(&root, ACCOUNT).unwrap();
        let requirement_before = stored_file(&root, "worker.requirement").unwrap();
        let socket_before =
            rustix::fs::statat(&root, "worker.sock", rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .unwrap();
        let managed =
            crate::launchd::inspect(&format!("gui/{}/tech.fixedwidth.glass", account.owner_uid))
                .unwrap()
                .unwrap();
        let managed_pid = crate::setup::checked_loaded_agent_pid(&managed)
            .unwrap()
            .unwrap();
        let control = private_session::account_processes(account.owner_uid)
            .unwrap()
            .into_iter()
            .find(|process| process.matches_pid(managed_pid))
            .unwrap();
        let requirement = requirement_before.as_ref().unwrap();
        assert!(
            private_session::process_matches_requirement(account.owner_uid, &control, requirement)
                .unwrap()
        );
        let refusal = inspect_processes_with(account.owner_uid, requirement, || Ok(vec![control]))
            .unwrap_err();
        assert!(refusal.to_string().contains("a Glass process still runs"));
        let started = Instant::now();
        for label in [LABEL, ONBOARD_LABEL, LABEL, LABEL, ONBOARD_LABEL] {
            assert!(
                inspect(&format!("gui/{}/{label}", account.uid), &account, &root)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(stored_file(&root, ACCOUNT).unwrap(), account_before);
        assert_eq!(
            stored_file(&root, "worker.requirement").unwrap(),
            requirement_before
        );
        let socket_after =
            rustix::fs::statat(&root, "worker.sock", rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .unwrap();
        assert_eq!(
            (socket_after.st_dev, socket_after.st_ino),
            (socket_before.st_dev, socket_before.st_ino)
        );
        println!(
            "logged-out worker/helper removal proof passed in {:?}; registration bytes and socket unchanged",
            started.elapsed()
        );
    }
}
