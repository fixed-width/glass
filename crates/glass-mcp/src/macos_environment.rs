//! One-time account enrollment; per-app work remains behind the normal backend factory.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};
use glass_macos::private_session;
use rustix::fs::{Mode, OFlags, open, openat};
use serde::{Deserialize, Serialize};

const LABEL: &str = "tech.fixedwidth.glass.worker";
const ACCOUNT: &str = "account.plist";
const JOB: &str = "tech.fixedwidth.glass.worker.plist";
const ONBOARD_LABEL: &str = "tech.fixedwidth.glass.onboarding";
const ONBOARD_JOB: &str = "tech.fixedwidth.glass.onboarding.plist";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    version: u32,
    uid: u32,
    gid: u32,
    record_uuid: String,
    name: String,
    home: PathBuf,
    owner_uid: u32,
}

fn checked(command: &mut Command) -> anyhow::Result<Vec<u8>> {
    let result = command.output()?;
    ensure!(
        result.status.success(),
        "macOS refused {}",
        command.get_program().to_string_lossy()
    );
    ensure!(
        result.stdout.len() <= 1024 * 1024,
        "unexpected system response size"
    );
    Ok(result.stdout)
}

fn account_record(name: &str, owner: u32, enrollment: bool) -> anyhow::Result<Enrollment> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "invalid account short name"
    );
    let mut command = Command::new("/usr/bin/dscl");
    command.args([
        "-plist",
        "/Local/Default",
        "-read",
        &format!("/Users/{name}"),
        "UniqueID",
        "PrimaryGroupID",
        "GeneratedUID",
        "NFSHomeDirectory",
    ]);
    if enrollment {
        command.arg("AuthenticationAuthority");
    }
    let result = command.output()?;
    let bytes = crate::macos_account::account_lookup(
        name,
        result.status.success(),
        result.stdout,
        &result.stderr,
    )?;
    let attributes: plist::Dictionary = plist::from_bytes(&bytes)?;
    let one = |key: &str| -> anyhow::Result<&str> {
        let values = attributes
            .get(&format!("dsAttrTypeStandard:{key}"))
            .and_then(plist::Value::as_array)
            .context("account attribute missing")?;
        ensure!(values.len() == 1, "ambiguous account attribute");
        values[0].as_string().context("invalid account attribute")
    };
    let account = Enrollment {
        version: 1,
        uid: one("UniqueID")?.parse()?,
        gid: one("PrimaryGroupID")?.parse()?,
        record_uuid: one("GeneratedUID")?.to_owned(),
        name: name.to_owned(),
        home: PathBuf::from(one("NFSHomeDirectory")?),
        owner_uid: owner,
    };
    ensure!(
        owner >= 501 && account.uid >= 501 && account.uid != owner && account.gid != 0,
        "select a separate standard account"
    );
    ensure!(
        account.home.is_absolute() && account.home != Path::new("/"),
        "account home must be absolute"
    );
    if enrollment {
        account_creation::check_created_account(&account, true)?;
        glass_macos::account_setup::verify_identity(
            &account.name,
            &account.record_uuid,
            account.uid,
        )
        .map_err(anyhow::Error::msg)?;
        let authorities = attributes
            .get("dsAttrTypeStandard:AuthenticationAuthority")
            .and_then(plist::Value::as_array)
            .context("account has no login authority")?;
        ensure!(
            !authorities.is_empty()
                && authorities.iter().all(|value| value
                    .as_string()
                    .is_some_and(|value| !value.starts_with(";DisabledUser;"))),
            "account login is disabled"
        );
        let membership = checked(
            Command::new("/usr/bin/dsmemberutil")
                .args(["checkmembership", "-U", name, "-G", "admin"])
                .env("LC_ALL", "C"),
        )?;
        ensure!(
            String::from_utf8(membership)?
                .trim_end()
                .ends_with("is not a member of the group"),
            "select a standard account, not an administrator"
        );
    }
    Ok(account)
}

/// Walk through retained directory descriptors, rejecting symlinks and writable parents.
fn directory(path: &Path, owner: u32, create: bool) -> anyhow::Result<File> {
    ensure!(path.is_absolute(), "setup path is not absolute");
    let mut fd = File::from(open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    for part in path.components().skip(1) {
        let Component::Normal(name) = part else {
            bail!("setup path contains nonliteral components")
        };
        let next = openat(
            &fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        );
        let next = match next {
            Err(rustix::io::Errno::NOENT) if create => {
                rustix::fs::mkdirat(&fd, name, Mode::from_raw_mode(0o755))?;
                let child = File::from(openat(
                    &fd,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                rustix::fs::fchown(&child, Some(rustix::process::Uid::from_raw(owner)), None)?;
                rustix::fs::fchmod(&child, Mode::from_raw_mode(0o755))?;
                child
            }
            result => File::from(result?),
        };
        let info = next.metadata()?;
        ensure!(
            (info.uid() == 0 || info.uid() == owner) && info.mode() & 0o022 == 0,
            "unprotected setup directory"
        );
        private_session::require_write_protected(&next, info.uid())?;
        fd = next;
    }
    Ok(fd)
}

fn owner_root() -> anyhow::Result<PathBuf> {
    let owner = if rustix::process::geteuid().is_root() {
        root_owner()?
    } else {
        rustix::process::geteuid().as_raw()
    };
    let root = private_session::root_for(owner);
    if root.try_exists()? {
        return Ok(root);
    }
    let legacy = Path::new(private_session::ROOT);
    if legacy.join(ACCOUNT).try_exists()? && private_session::legacy_owner()? == owner {
        return Ok(legacy.to_path_buf());
    }
    Ok(root)
}

fn read_enrollment() -> anyhow::Result<Enrollment> {
    read_enrollment_at(&owner_root()?)
}

fn read_enrollment_at(path: &Path) -> anyhow::Result<Enrollment> {
    let root = directory(path, 0, false)?;
    let file = File::from(openat(
        &root,
        ACCOUNT,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let info = file.metadata()?;
    ensure!(
        info.is_file()
            && info.uid() == 0
            && info.mode() & 0o022 == 0
            && info.nlink() == 1
            && info.len() <= 4096,
        "invalid account enrollment file"
    );
    private_session::require_write_protected(&file, 0)?;
    let value: Enrollment = plist::from_reader(file)?;
    ensure!(
        value == account_record(&value.name, value.owner_uid, false)?,
        "enrolled account identity changed"
    );
    Ok(value)
}

fn job(account: &Enrollment, binary: &Path) -> anyhow::Result<Vec<u8>> {
    let mut value = plist::Dictionary::new();
    value.insert("Label".into(), LABEL.into());
    value.insert(
        "ProgramArguments".into(),
        plist::Value::Array(vec![
            binary.to_string_lossy().into_owned().into(),
            "macos-worker".into(),
            account.owner_uid.to_string().into(),
        ]),
    );
    value.insert(
        "WorkingDirectory".into(),
        account.home.to_string_lossy().into_owned().into(),
    );
    value.insert("RunAtLoad".into(), false.into());
    value.insert("KeepAlive".into(), false.into());
    value.insert("LimitLoadToSessionType".into(), "Aqua".into());
    value.insert("ProcessType".into(), "Interactive".into());
    value.insert("ExitTimeOut".into(), 10.into());
    let mut worker = plist::Dictionary::new();
    worker.insert(
        "SockPathName".into(),
        private_session::root_for(account.owner_uid)
            .join("worker.sock")
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    worker.insert("SockPathMode".into(), 0o600.into());
    let mut sockets = plist::Dictionary::new();
    sockets.insert("Worker".into(), worker.into());
    value.insert("Sockets".into(), sockets.into());
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &value)?;
    Ok(bytes)
}

fn onboarding_job(account: &Enrollment, binary: &Path) -> anyhow::Result<Vec<u8>> {
    let mut value = plist::Dictionary::new();
    value.insert("Label".into(), ONBOARD_LABEL.into());
    value.insert(
        "ProgramArguments".into(),
        plist::Value::Array(vec![
            binary.to_string_lossy().into_owned().into(),
            "macos-onboarding".into(),
            account.owner_uid.to_string().into(),
        ]),
    );
    value.insert(
        "WorkingDirectory".into(),
        account.home.to_string_lossy().into_owned().into(),
    );
    value.insert("LimitLoadToSessionType".into(), "Aqua".into());
    value.insert("ProcessType".into(), "Interactive".into());
    value.insert("RunAtLoad".into(), true.into());
    value.insert("KeepAlive".into(), false.into());
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &value)?;
    Ok(bytes)
}

fn install_onboarding(
    account: &Enrollment,
    binary: &Path,
    root: &File,
    agents: &File,
) -> anyhow::Result<()> {
    let bytes = onboarding_job(account, binary)?;
    if !same_file(agents, ONBOARD_JOB, &bytes)? {
        create_file(agents, ONBOARD_JOB, &bytes)?;
    }
    if !same_file(root, ONBOARD_JOB, &bytes)? {
        create_file(root, ONBOARD_JOB, &bytes)?;
    }
    let domain = format!("gui/{}", account.uid);
    let target = format!("{domain}/{ONBOARD_LABEL}");
    match crate::launchd::inspect_for_setup(&target)? {
        crate::launchd::Inspection::Loaded(loaded) => {
            ensure!(
                loaded_job_matches(
                    &loaded,
                    &[
                        &binary.to_string_lossy(),
                        "macos-onboarding",
                        &account.owner_uid.to_string()
                    ]
                ),
                "loaded permission helper belongs to another installation"
            );
            if crate::setup::checked_loaded_agent_pid(&loaded)?.is_none() {
                checked(Command::new("/bin/launchctl").args(["kickstart", &target]))?;
            }
        }
        crate::launchd::Inspection::MissingJob => {
            checked(
                Command::new("/bin/launchctl")
                    .arg("bootstrap")
                    .arg(domain)
                    .arg(account.home.join("Library/LaunchAgents").join(ONBOARD_JOB)),
            )?;
        }
        crate::launchd::Inspection::MissingDomain | crate::launchd::Inspection::GuiUnavailable => {
            eprintln!(
                "glass: permission onboarding will open when the development account logs in"
            );
        }
    }
    Ok(())
}

fn permission_helper_registered(account: &Enrollment) -> anyhow::Result<bool> {
    let root = directory(&private_session::root_for(account.owner_uid), 0, false)?;
    let binary = std::env::current_exe()?.canonicalize()?;
    same_file(&root, ONBOARD_JOB, &onboarding_job(account, &binary)?)
}

fn onboarding_state(owner: u32) -> anyhow::Result<Option<(String, bool)>> {
    ensure!(
        private_session::owner_uid() == owner,
        "permission request has no attached owner"
    );
    let root_path = private_session::root_for(owner);
    let account = read_enrollment_at(&root_path)?;
    ensure!(
        account.uid == rustix::process::getuid().as_raw(),
        "permission request is not in the enrolled account"
    );
    let root = directory(&root_path, 0, false)?;
    if stored_file(&root, ONBOARD_JOB)?.is_none() {
        return Ok(None);
    }
    let agents = directory(
        &account.home.join("Library/LaunchAgents"),
        account.uid,
        false,
    )?;
    let binary = std::env::current_exe()?.canonicalize()?;
    let expected = onboarding_job(&account, &binary)?;
    ensure!(
        same_file(&root, ONBOARD_JOB, &expected)? && same_file(&agents, ONBOARD_JOB, &expected)?,
        "permission helper is not registered; open Glass application setup to repair it"
    );
    let target = format!("gui/{}/{ONBOARD_LABEL}", account.uid);
    let loaded = Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()?;
    ensure!(
        loaded.status.success()
            && loaded_job_matches(
                &String::from_utf8_lossy(&loaded.stdout),
                &[
                    &binary.to_string_lossy(),
                    "macos-onboarding",
                    &owner.to_string()
                ]
            ),
        "permission helper is not bound to this installation"
    );
    let running =
        crate::setup::checked_loaded_agent_pid(&String::from_utf8_lossy(&loaded.stdout))?.is_some();
    Ok(Some((target, running)))
}

pub(crate) fn onboarding_running(owner: u32) -> anyhow::Result<bool> {
    Ok(onboarding_state(owner)?.is_some_and(|(_, running)| running))
}

pub(crate) fn request_onboarding(owner: u32) -> anyhow::Result<()> {
    let (target, running) = onboarding_state(owner)?.context(
        "permission helper is not registered; open Glass application setup to repair it",
    )?;
    if !running {
        checked(Command::new("/bin/launchctl").args(["kickstart", &target]))?;
    }
    Ok(())
}

fn validate_socket(info: &rustix::fs::Stat, owner: u32, worker: u32) -> anyhow::Result<()> {
    ensure!(
        rustix::fs::FileType::from_raw_mode(info.st_mode) == rustix::fs::FileType::Socket
            && info.st_nlink == 1
            && info.st_mode & 0o777 == 0o600
            && [0, owner, worker].contains(&info.st_uid),
        "private worker socket has unexpected identity or permissions"
    );
    Ok(())
}

fn remove_socket(parent: &File, owner: u32, worker: u32) -> anyhow::Result<()> {
    use rustix::fs::{AtFlags, statat, unlinkat};

    let info = match statat(parent, "worker.sock", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(info) => info,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    validate_socket(&info, owner, worker)?;
    match unlinkat(parent, "worker.sock", AtFlags::empty()) {
        Ok(()) => parent.sync_all()?,
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn unregister_worker(
    parent: &File,
    account: &Enrollment,
    unregister: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    unregister()?;
    remove_socket(parent, account.owner_uid, account.uid)
}

fn normalize_socket(parent: &File, owner: u32, worker: u32) -> anyhow::Result<()> {
    use rustix::fs::{AtFlags, FileType, chownat, statat};

    let before = statat(parent, "worker.sock", AtFlags::SYMLINK_NOFOLLOW)?;
    validate_socket(&before, owner, worker)?;
    if before.st_uid != owner {
        chownat(
            parent,
            "worker.sock",
            Some(rustix::process::Uid::from_raw(owner)),
            None,
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
    }
    let after = statat(parent, "worker.sock", AtFlags::SYMLINK_NOFOLLOW)?;
    ensure!(
        before.st_dev == after.st_dev
            && before.st_ino == after.st_ino
            && FileType::from_raw_mode(after.st_mode) == FileType::Socket
            && after.st_uid == owner
            && after.st_mode & 0o777 == 0o600,
        "private worker socket changed while assigning owner access"
    );
    Ok(())
}

fn wait_for_socket(parent: &File, owner: u32, worker: u32) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match rustix::fs::statat(parent, "worker.sock", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => return normalize_socket(parent, owner, worker),
            Err(rustix::io::Errno::NOENT) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(error)
                    .context("launchd did not create the private worker activation socket");
            }
        }
    }
}

fn enable_worker(
    root: &File,
    account: &Enrollment,
    register: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    // The worker must pass enrollment checks before checking in to create its socket.
    match rustix::fs::unlinkat(root, "disabled", rustix::fs::AtFlags::empty()) {
        Ok(()) => root.sync_all()?,
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    if register()? {
        wait_for_socket(root, account.owner_uid, account.uid)?;
    }
    Ok(())
}

fn create_file(parent: &File, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let temporary = format!(".setup-{:032x}", rand::random::<u128>());
    let mut file = File::from(openat(
        parent,
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let result = (|| {
        private_session::require_write_protected(&file, 0)?;
        file.write_all(bytes)?;
        rustix::fs::fchmod(&file, Mode::from_raw_mode(0o644))?;
        file.sync_all()?;
        rustix::fs::renameat_with(
            parent,
            &temporary,
            parent,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        parent.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(parent, &temporary, rustix::fs::AtFlags::empty());
    }
    result
}

fn stored_file(parent: &File, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let file = match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        result => File::from(result?),
    };
    let info = file.metadata()?;
    ensure!(
        info.is_file()
            && info.uid() == 0
            && info.nlink() == 1
            && info.mode() & 0o022 == 0
            && info.len() <= 65536,
        "setup file was replaced or is unprotected"
    );
    private_session::require_write_protected(&file, 0)?;
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "setup file grew while reading");
    Ok(Some(bytes))
}

fn same_file(parent: &File, name: &str, bytes: &[u8]) -> anyhow::Result<bool> {
    let Some(actual) = stored_file(parent, name)? else {
        return Ok(false);
    };
    ensure!(
        actual == bytes,
        "existing Glass setup differs; refusing to replace its registered files"
    );
    Ok(true)
}

fn root_owner() -> anyhow::Result<u32> {
    ensure!(
        rustix::process::getuid().is_root() && rustix::process::geteuid().is_root(),
        "account registration requires local administrator authentication"
    );
    let owner = std::env::var("SUDO_UID")
        .context("run setup from your ordinary account using sudo authentication")?
        .parse()?;
    ensure!(owner >= 501, "setup requires an ordinary account owner");
    Ok(owner)
}

fn loaded_job_matches(output: &str, expected: &[&str]) -> bool {
    let mut depth = 0usize;
    let mut program = None;
    let mut arguments = None;
    let mut collecting = false;
    for line in output.lines().map(str::trim) {
        if line == "}" {
            if depth == 2 {
                collecting = false;
            }
            let Some(next) = depth.checked_sub(1) else {
                return false;
            };
            depth = next;
        } else if depth == 1 {
            if let Some(value) = line.strip_prefix("program = ")
                && program.replace(value).is_some()
            {
                return false;
            }
            if line == "arguments = {" {
                if arguments.replace(Vec::new()).is_some() {
                    return false;
                }
                collecting = true;
            }
        } else if depth == 2
            && collecting
            && let Some(arguments) = &mut arguments
        {
            arguments.push(line);
        }
        if line.ends_with(" = {") || line.ends_with(" => {") {
            depth += 1;
        }
    }
    depth == 0 && program == expected.first().copied() && arguments.as_deref() == Some(expected)
}

fn stop_registered_worker(target: &str, account: &Enrollment) -> anyhow::Result<()> {
    crate::launchd::stop(target, account.uid)
}

fn legacy_job(account: &Enrollment, binary: &Path) -> anyhow::Result<Vec<u8>> {
    let mut value: plist::Dictionary = plist::from_bytes(&job(account, binary)?)?;
    value.insert(
        "ProgramArguments".into(),
        plist::Value::Array(vec![
            binary.to_string_lossy().into_owned().into(),
            "macos-worker".into(),
        ]),
    );
    let mut socket = plist::Dictionary::new();
    socket.insert(
        "SockPathName".into(),
        Path::new(private_session::ROOT)
            .join("worker.sock")
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    socket.insert("SockPathMode".into(), 0o600.into());
    let mut sockets = plist::Dictionary::new();
    sockets.insert("Worker".into(), socket.into());
    value.insert("Sockets".into(), sockets.into());
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &value)?;
    Ok(bytes)
}

fn migrate_legacy(account: &Enrollment, binary: &Path) -> anyhow::Result<()> {
    let legacy_path = Path::new(private_session::ROOT);
    if !legacy_path.join(ACCOUNT).try_exists()? {
        return Ok(());
    }
    let existing = read_enrollment_at(legacy_path)?;
    // Other owners keep their legacy registration until they choose to migrate it.
    if existing.owner_uid != account.owner_uid {
        return Ok(());
    }
    ensure!(
        existing == *account,
        "legacy enrollment names another development account"
    );
    let legacy = directory(legacy_path, 0, false)?;
    fs4::FileExt::try_lock(&legacy).context("legacy registration is already changing")?;
    let lease = File::from(openat(
        &legacy,
        ACCOUNT,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    fs4::FileExt::try_lock(&lease).context("stop active Glass work before migrating enrollment")?;
    let agents = directory(
        &account.home.join("Library/LaunchAgents"),
        account.uid,
        false,
    )?;
    let old = legacy_job(account, binary)?;
    let registered_file = same_file(&agents, JOB, &old)?;
    let target = format!("gui/{}/{LABEL}", account.uid);
    let loaded_job = crate::launchd::inspect(&target)?;
    let loaded = loaded_job.is_some();
    ensure!(
        registered_file || !loaded,
        "legacy job cannot be attributed; registration retained"
    );
    if let Some(loaded_job) = loaded_job {
        ensure!(
            loaded_job_matches(&loaded_job, &[&binary.to_string_lossy(), "macos-worker"]),
            "loaded legacy worker belongs to another installation; registration retained"
        );
    }
    let bytes = stored_file(&legacy, ACCOUNT)?.context("legacy account record missing")?;
    let requirement =
        stored_file(&legacy, "worker.requirement")?.context("legacy worker requirement missing")?;
    let _new_root = prepare_registration(account, &bytes, &requirement)?;
    if !same_file(&legacy, "disabled", b"Glass account registration removed\n")? {
        create_file(&legacy, "disabled", b"Glass account registration removed\n")?;
    }
    unregister_worker(&legacy, account, || {
        if loaded {
            stop_registered_worker(&target, account)?;
        }
        Ok(())
    })?;
    if registered_file {
        rustix::fs::unlinkat(&agents, JOB, rustix::fs::AtFlags::empty())?;
        agents.sync_all()?;
    }
    for name in [ACCOUNT, "worker.requirement", "disabled"] {
        rustix::fs::unlinkat(&legacy, name, rustix::fs::AtFlags::empty())?;
    }
    legacy.sync_all()?;
    Ok(())
}

fn setup_suffix(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 32
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn staged_names(parent: &File, owner: u32) -> anyhow::Result<Vec<String>> {
    let prefix = format!(".setup-{owner}-");
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(parent)? {
        let entry = entry?;
        if let Ok(name) = entry.file_name().to_str()
            && setup_suffix(name, &prefix)
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

fn registration_files(root: &File) -> anyhow::Result<Vec<String>> {
    use rustix::fs::{AtFlags, Dir, FileType};
    let mut names = Vec::new();
    for entry in Dir::read_from(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy();
        if name == "." || name == ".." {
            continue;
        }
        let info = rustix::fs::statat(root, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)?;
        ensure!(
            ([ACCOUNT, "worker.requirement", "disabled", ONBOARD_JOB].contains(&name.as_ref())
                || setup_suffix(&name, ".setup-"))
                && FileType::from_raw_mode(info.st_mode) == FileType::RegularFile
                && info.st_uid == root.metadata()?.uid()
                && info.st_nlink == 1
                && info.st_mode & 0o133 == 0
                && info.st_size <= 65536,
            "unexpected enrollment contents; disabled registration retained"
        );
        names.push(name.into_owned());
    }
    Ok(names)
}

fn cleanup_retired(parent: &File, name: &str) -> anyhow::Result<()> {
    use rustix::fs::AtFlags;
    let root = match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        result => File::from(result?),
    };
    ensure!(
        root.metadata()?.uid() == parent.metadata()?.uid(),
        "retired registration owner changed"
    );
    fs4::FileExt::try_lock(&root).context("registration cleanup is already running")?;
    for file in registration_files(&root)? {
        rustix::fs::unlinkat(&root, file.as_str(), AtFlags::empty())?;
    }
    root.sync_all()?;
    rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    parent.sync_all()?;
    Ok(())
}

fn retire_registration(parent: &File, name: &str, root: &File) -> anyhow::Result<()> {
    registration_files(root)?;
    let retired = format!(".unenroll-{name}");
    cleanup_retired(parent, &retired)?;
    let current = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
    let expected = root.metadata()?;
    ensure!(
        current.st_dev as u64 == expected.dev() && current.st_ino == expected.ino(),
        "registration identity changed before removal"
    );
    rustix::fs::renameat_with(
        parent,
        name,
        parent,
        retired.as_str(),
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    parent.sync_all()?;
    // The runtime and permission cleanup is complete before this explicit route transition.
    // Reuse the held descriptor's lock through cleanup; a separate open would conflict with it.
    for file in registration_files(root)? {
        rustix::fs::unlinkat(root, file.as_str(), rustix::fs::AtFlags::empty())?;
    }
    root.sync_all()?;
    rustix::fs::unlinkat(parent, retired.as_str(), rustix::fs::AtFlags::REMOVEDIR)?;
    parent.sync_all()?;
    Ok(())
}

fn finish_removal(path: &Path, root: &File) -> anyhow::Result<()> {
    let parent = directory(
        path.parent().context("registration parent unavailable")?,
        0,
        false,
    )?;
    retire_registration(
        &parent,
        path.file_name()
            .and_then(|name| name.to_str())
            .context("registration owner unavailable")?,
        root,
    )
    .context("removing account registration; inspect status if retired metadata cleanup failed")
}

fn mutation_lock(kind: &str, identity: u32) -> anyhow::Result<File> {
    let parent = directory(&Path::new(private_session::ROOT).join("accounts"), 0, true)?;
    let lock = File::from(openat(
        &parent,
        format!(".{kind}-{identity}.lock").as_str(),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let info = lock.metadata()?;
    ensure!(
        info.is_file() && info.uid() == 0 && info.nlink() == 1 && info.mode() & 0o077 == 0,
        "registration lock is not protected"
    );
    private_session::require_write_protected(&lock, 0)?;
    fs4::FileExt::try_lock(&lock).context("account registration is already changing")?;
    Ok(lock)
}

fn accepts_job(parent: &File, current: &[u8], legacy: &[u8]) -> anyhow::Result<bool> {
    let Some(actual) = stored_file(parent, JOB)? else {
        return Ok(false);
    };
    ensure!(
        actual == current || actual == legacy,
        "development account is already enrolled by another user or installation"
    );
    Ok(true)
}

fn publish_registration(parent: &File, name: &str, staged: &str) -> anyhow::Result<()> {
    rustix::fs::renameat_with(
        parent,
        staged,
        parent,
        name,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    parent.sync_all()?;
    Ok(())
}

fn registration_permissions(root: &File) -> anyhow::Result<()> {
    rustix::fs::fchmod(root, Mode::from_raw_mode(0o755))?;
    root.sync_all()?;
    Ok(())
}

fn stage_registration(
    parent: &File,
    owner: &str,
    prepare: impl FnOnce(&File) -> anyhow::Result<()>,
) -> anyhow::Result<File> {
    let staged = format!(".setup-{owner}-{:032x}", rand::random::<u128>());
    rustix::fs::mkdirat(parent, staged.as_str(), Mode::from_raw_mode(0o755))?;
    let root = File::from(openat(
        parent,
        staged.as_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let prepared = prepare(&root).and_then(|()| {
        registration_permissions(&root)?;
        publish_registration(parent, owner, &staged)
    });
    if let Err(primary) = prepared {
        let cleanup = cleanup_retired(parent, &staged);
        return match cleanup {
            Ok(()) => Err(primary),
            Err(cleanup) => {
                Err(primary.context(format!("staged registration cleanup: {cleanup:#}")))
            }
        };
    }
    Ok(root)
}

fn prepare_registration(
    account: &Enrollment,
    bytes: &[u8],
    requirement: &[u8],
) -> anyhow::Result<File> {
    let path = private_session::root_for(account.owner_uid);
    if path.try_exists()? {
        let root = directory(&path, 0, false)?;
        ensure!(
            same_file(&root, ACCOUNT, bytes)?
                && same_file(&root, "worker.requirement", requirement)?,
            "existing account registration is incomplete"
        );
        registration_permissions(&root)?;
        return Ok(root);
    }
    let parent = directory(
        path.parent().context("registration parent unavailable")?,
        0,
        true,
    )?;
    stage_registration(&parent, &account.owner_uid.to_string(), |root| {
        create_file(root, "disabled", b"Glass account registration removed\n")?;
        create_file(root, "worker.requirement", requirement)?;
        create_file(root, ACCOUNT, bytes)
    })
}

fn prepare_home(account: &Enrollment) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(&account.home) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                account.home == Path::new("/Users").join(&account.name),
                "automatic home preparation supports the standard /Users account location; prepare this account's custom home first"
            );
            directory(Path::new("/Users"), 0, false)?;
            checked(Command::new("/usr/sbin/createhomedir").args([
                "-c",
                "-l",
                "-u",
                &account.name,
            ]))?;
            ensure!(
                *account == account_record(&account.name, account.owner_uid, true)?,
                "account identity changed during home preparation"
            );
        }
        Err(error) => return Err(error.into()),
    }
    let home = directory(&account.home, account.uid, false)
        .context("validating the selected account home")?;
    ensure!(
        home.metadata()?.uid() == account.uid,
        "the selected account does not own its home"
    );
    Ok(())
}

/// Authenticated installer entry; never creates/deletes accounts or changes their credentials.
pub fn enroll(name: &str) -> anyhow::Result<()> {
    enroll_with_options(name, false, false)
}

pub fn enroll_with_options(name: &str, onboarding: bool, repair: bool) -> anyhow::Result<()> {
    let owner = root_owner()?;
    let account = account_record(name, owner, true)?;
    let _owner_lock = mutation_lock("owner", owner)?;
    let _worker_lock = mutation_lock("worker", account.uid)?;
    let binary = std::env::current_exe()?.canonicalize()?;
    ensure!(
        crate::setup::is_inside_app_bundle(&binary),
        "install GlassMcp.app before enrolling an account"
    );
    checked(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(crate::setup::app_bundle_path(&binary)),
    )?;
    prepare_home(&account)?;
    if Path::new(private_session::ROOT)
        .join(ACCOUNT)
        .try_exists()?
        && let Ok(legacy) = read_enrollment_at(Path::new(private_session::ROOT))
    {
        ensure!(
            legacy.uid != account.uid || legacy.owner_uid == owner,
            "the development account is already enrolled by another user"
        );
    }
    let agents = directory(
        &account.home.join("Library/LaunchAgents"),
        account.uid,
        true,
    )?;
    accepts_job(
        &agents,
        &job(&account, &binary)?,
        &legacy_job(&account, &binary)?,
    )?;
    migrate_legacy(&account, &binary)?;
    let path = private_session::root_for(owner);
    let bytes = if path.try_exists()? {
        let bytes = stored_file(&directory(&path, 0, false)?, ACCOUNT)?
            .context("account record unavailable")?;
        ensure!(
            plist::from_bytes::<Enrollment>(&bytes)? == account,
            "account identity changed"
        );
        bytes
    } else {
        let mut bytes = Vec::new();
        plist::to_writer_xml(&mut bytes, &account)?;
        bytes
    };
    let requirement = private_session::signing_requirement()?;
    let root = prepare_registration(&account, &bytes, &requirement)?;
    fs4::FileExt::try_lock(&root).context("account setup is already changing")?;
    let lease = File::from(openat(
        &root,
        ACCOUNT,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    fs4::FileExt::try_lock(&lease)
        .context("Glass application work is active; stop it before changing setup")?;
    let job_bytes = job(&account, &binary)?;
    if !same_file(&agents, JOB, &job_bytes)? {
        create_file(&agents, JOB, &job_bytes)?;
    }
    let domain = format!("gui/{}", account.uid);
    let target = format!("{domain}/{LABEL}");
    if repair
        && let crate::launchd::Inspection::Loaded(loaded) =
            crate::launchd::inspect_for_setup(&target)?
    {
        ensure!(
            loaded_job_matches(
                &loaded,
                &[
                    &binary.to_string_lossy(),
                    "macos-worker",
                    &account.owner_uid.to_string()
                ]
            ),
            "loaded worker belongs to another installation; registration retained"
        );
        ensure!(
            crate::setup::checked_loaded_agent_pid(&loaded)?.is_none(),
            "the worker process is still running; its registration was retained"
        );
        stop_registered_worker(&target, &account)?;
    }
    if repair {
        remove_socket(&root, account.owner_uid, account.uid)?;
    }
    enable_worker(&root, &account, || {
        let registered = match crate::launchd::inspect_for_setup(&target)? {
            crate::launchd::Inspection::Loaded(_) => true,
            crate::launchd::Inspection::MissingJob => {
                // launchd leaves its socket node behind when a registration is removed.
                remove_socket(&root, account.owner_uid, account.uid)?;
                checked(
                    Command::new("/bin/launchctl")
                        .arg("bootstrap")
                        .arg(&domain)
                        .arg(account.home.join("Library/LaunchAgents").join(JOB)),
                )?;
                true
            }
            crate::launchd::Inspection::MissingDomain
            | crate::launchd::Inspection::GuiUnavailable => {
                eprintln!(
                    "glass: registration saved; log into the selected account once to load its worker"
                );
                false
            }
        };
        if registered {
            worker_activation::start_if_missing(&root, &account, &binary)?;
        }
        Ok(registered)
    })?;
    if onboarding {
        install_onboarding(&account, &binary, &root, &agents)?;
    }
    println!(
        "glass: enrolled {}; account, home and login credentials remain yours",
        account.name
    );
    Ok(())
}

fn elevate_using(
    arguments: &[&str],
    non_interactive: bool,
    system_dialog: bool,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let status = if non_interactive || (!system_dialog && std::io::stdin().is_terminal()) {
        let mut command = Command::new("/usr/bin/sudo");
        if non_interactive {
            command.arg("-n");
        }
        command
            .arg(std::env::current_exe()?)
            .args(arguments)
            .status()?
    } else {
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        let mut words = vec![
            "/usr/bin/env".to_owned(),
            format!("SUDO_UID={}", rustix::process::getuid().as_raw()),
            std::env::current_exe()?.to_string_lossy().into_owned(),
        ];
        words.extend(arguments.iter().map(|value| (*value).to_owned()));
        let script = words
            .iter()
            .map(|value| quote(value))
            .collect::<Vec<_>>()
            .join(" ");
        let expression = format!(
            "do shell script {} with administrator privileges",
            serde_json::to_string(&script)?
        );
        let output = Command::new("/usr/bin/osascript")
            .args(["-e", &expression])
            .output()?;
        ensure!(
            output.status.success(),
            "macOS account setup did not complete: {}",
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(8192)
                .collect::<String>()
        );
        output.status
    };
    ensure!(
        status.success(),
        "macOS account setup did not complete; existing account and grants were retained"
    );
    Ok(())
}

mod account_creation;
mod desktop_start;
mod guided;
mod worker_activation;

/// Internal privileged startup role; the account password never appears in arguments.
pub fn start_environment(endpoint: &Path) -> anyhow::Result<()> {
    desktop_start::privileged(endpoint)
}

/// Internal, authenticated root role; credentials arrive through its verified local connection.
pub fn prepare_account(name: &str, endpoint: &Path) -> anyhow::Result<()> {
    account_creation::prepare(name, endpoint)
}

pub(crate) fn application_lease() -> anyhow::Result<crate::macos_setup::Lease> {
    ensure!(
        !desktop_start::pending(rustix::process::getuid().as_raw())?,
        "Glass desktop startup or temporary-access cleanup needs attention; open application setup and choose Start environment"
    );
    guided::environment_lease(crate::macos_setup::LeaseKind::Application)
}

pub fn setup(
    name: &str,
    non_interactive: bool,
    launchagent: bool,
    addr: Option<&str>,
) -> anyhow::Result<()> {
    guided::run(
        guided::Options {
            name,
            create: None,
            non_interactive,
            launchagent,
            addr,
            system_dialog: false,
            cancelled: &std::sync::atomic::AtomicBool::new(false),
            resume: None,
            previous: None,
        },
        |progress| println!("glass: {}", progress.message),
    )
}

pub(crate) fn resume_setup(
    non_interactive: bool,
    integration: Option<crate::setup::RunMode>,
    addr: Option<&str>,
) -> anyhow::Result<()> {
    let name = read_enrollment()?.name;
    let launchagent = match integration {
        Some(crate::setup::RunMode::Http) => true,
        Some(crate::setup::RunMode::Stdio) => false,
        None => crate::setup::launch_agent_plist_path(&std::env::var("HOME")?).try_exists()?,
    };
    setup(&name, non_interactive, launchagent, addr)
}

pub(crate) fn has_pending_setup() -> anyhow::Result<bool> {
    guided::saved().map(|saved| saved.is_some())
}

/// The app and CLI use the same enrollment, consent and readiness coordinator.
pub fn show_setup(addr: &str) -> anyhow::Result<()> {
    use glass_macos::setup_window::{ResumeCreation, SetupActions, SetupProgress};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    let (saved, mut saved_error) = match guided::saved() {
        Ok(saved) => (saved, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let automatic = match saved.as_ref().map(guided::can_resume).transpose() {
        Ok(Some(true)) => saved.clone(),
        Ok(_) => None,
        Err(error) => {
            saved_error = Some(format!("{error:#}"));
            None
        }
    };
    let existing = match owner_root().and_then(|root| root.try_exists().map_err(Into::into)) {
        Ok(true) => Some(read_enrollment()),
        Ok(false) => None,
        Err(error) => Some(Err(error)),
    };
    let enrolled_name = existing
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(|account| account.name.clone());
    let diagnose =
        enrolled_name.is_some() && automatic.is_none() && saved.is_none() && saved_error.is_none();
    let creation = if existing.is_none() {
        account_creation::last_created(rustix::process::getuid().as_raw())
    } else {
        Ok(None)
    };
    let resume_creation = if existing.is_none() && automatic.is_none() && saved_error.is_none() {
        if let Some(saved) = &saved
            && saved.create
        {
            match account_creation::completed_identity(saved.owner, &saved.name) {
                Ok(None) => Some(ResumeCreation {
                    name: saved.name.clone(),
                    attempt: saved.id.clone(),
                }),
                Ok(Some(_)) => None,
                Err(error) => {
                    saved_error = Some(format!("{error:#}"));
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };
    let account = crate::macos_setup::selected_account(
        enrolled_name.as_deref(),
        saved.as_ref().map(|saved| saved.name.as_str()),
        creation.as_ref().ok().and_then(|receipt| receipt.as_ref()),
    );
    let message = if let Some(Err(error)) = existing {
        format!(
            "Existing setup needs attention: {error:#}. Enter the development account name to check it. Glass will preserve unverified registration files."
        )
    } else if let Some(error) = saved_error {
        format!(
            "Saved setup needs attention: {error}. Choose Use existing account or Create account… to begin a new explicit attempt; completed account data are retained."
        )
    } else if let Some(saved) = &saved
        && automatic.is_none()
    {
        if resume_creation.is_some() {
            format!(
                "Account preparation for {} was interrupted. Enter the original chosen password in the secure dialog to continue; Glass did not save that password.",
                saved.name
            )
        } else {
            format!(
                "Saved setup for {} is paused. Choose Use existing account to check registration and continue, or Close to cancel the saved setup. Accounts, passwords and files are retained.",
                saved.name
            )
        }
    } else if let Err(error) = &creation {
        format!(
            "Saved account preparation needs attention: {error:#}. Existing records are retained."
        )
    } else if let Ok(Some(receipt)) = &creation {
        if receipt.created && receipt.password_set {
            format!(
                "A previous Glass setup created {}. Uninstall keeps development accounts and their files. Create a new account above, or enter an existing account's short name to reuse it.",
                receipt.name
            )
        } else {
            format!(
                "Account creation for {} is unfinished. Choose Create account… with the same name and original chosen password to resume.",
                receipt.name
            )
        }
    } else if account.is_empty() {
        "Create a standard development account here, or enter the short name of an existing one. Glass handles registration and permission checks.".into()
    } else {
        format!(
            "{account} is enrolled. Use existing account checks its live worker and resumes any pending setup. Disable isolation returns this user to shared-desktop use."
        )
    };
    let state = Arc::new(Mutex::new(SetupProgress {
        message,
        running: false,
        can_cancel: true,
        start_account: None,
    }));
    let cancelled = Arc::new(AtomicBool::new(false));
    let pending_id = Arc::new(Mutex::new(None::<String>));
    let begin_state = state.clone();
    let begin_cancelled = cancelled.clone();
    let begin_pending = pending_id.clone();
    let displayed_id = saved.as_ref().map(|saved| saved.id.clone());
    let addr = addr.to_owned();
    #[cfg(feature = "network")]
    let disable_addr = addr.clone();
    let disable_state = state.clone();
    let close_state = state.clone();
    let close_cancelled = cancelled.clone();
    let begin = Arc::new(
        move |name: String,
              create: Option<crate::macos_account::Password>,
              resume: Option<crate::macos_setup::pending::Pending>| {
            let mut state = match begin_state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            if state.running {
                return;
            }
            state.running = true;
            state.start_account = None;
            state.can_cancel = false;
            let previous = begin_pending
                .lock()
                .ok()
                .and_then(|id| id.clone())
                .or_else(|| displayed_id.clone());
            state.message =
                "Checking setup. Complete any macOS authentication locally; take your time.".into();
            drop(state);
            let state = begin_state.clone();
            let cancelled = begin_cancelled.clone();
            let pending_id = begin_pending.clone();
            let addr = resume
                .as_ref()
                .map(|saved| saved.addr.clone())
                .unwrap_or_else(|| addr.clone());
            let launchagent = resume
                .as_ref()
                .map(|saved| saved.launchagent)
                .unwrap_or(cfg!(feature = "network"));
            std::thread::spawn(move || {
                let result = guided::run(
                    guided::Options {
                        name: &name,
                        create,
                        non_interactive: false,
                        launchagent,
                        addr: Some(&addr),
                        system_dialog: true,
                        cancelled: &cancelled,
                        resume,
                        previous,
                    },
                    |progress| {
                        if let Ok(mut state) = state.lock() {
                            if let Some(id) = progress.pending_id
                                && let Ok(mut pending) = pending_id.lock()
                            {
                                *pending = Some(id);
                            }
                            state.message = progress.message;
                            state.can_cancel = progress.can_cancel;
                        }
                    },
                );
                if let Err(error) = &result {
                    let details: String = format!("{error:#}").chars().take(8192).collect();
                    eprintln!("glass: application setup failed: {details}");
                }
                if let Ok(mut state) = state.lock() {
                    state.running = false;
                    state.can_cancel = true;
                    state.message = match result {
                        Ok(()) => "Ready. Your MCP client can use the normal Glass endpoint. Keep the development account logged in; routine app runs need no account switching.".into(),
                        Err(error) if error.is::<crate::macos_setup::pending::Superseded>() => "This saved setup was cancelled or replaced. This window did not restart it. Close this window; begin a new setup explicitly if needed.".into(),
                        Err(error) if error.is::<crate::macos_account::MissingAccount>() => error.to_string(),
                        Err(error) if error.is::<guided::EnvironmentMissing>() => {
                            state.start_account = desktop_start::choice(&name).ok();
                            format!("The development desktop for {name} is not running. Choose Start environment… to start it here; Glass reuses existing registration and permissions.")
                        }
                        Err(error) if error.is::<guided::AccountCreationIncomplete>() => {
                            match account_creation::completed_identity(rustix::process::getuid().as_raw(), &name) {
                                Ok(Some(_)) => format!("{error:#}. The account and password are prepared. Choose Use existing account to finish registration; you do not need to enter the development-account password again."),
                                Ok(None) => format!("{error:#}. Completed account data and grants are retained. Choose Create account… with the same name and original chosen password to resume."),
                                Err(check) => format!("{error:#}. The saved account checkpoint could not be verified: {check:#}. Existing account data are retained; close and reopen application setup to check it."),
                            }
                        }
                        Err(error) => format!("{error:#}. Completed registration and grants are retained. Fix the reported cause, then choose Use existing account."),
                    };
                }
            });
        },
    );
    let create = begin.clone();
    let create_state = state.clone();
    let create_saved = saved.clone();
    let create_pending = pending_id.clone();
    let resume_begin = begin.clone();
    let start_state = state.clone();
    let start_cancelled = cancelled.clone();
    let on_start = Box::new(move |request: glass_macos::setup_window::StartAccount| {
        let password = match request
            .password
            .map(crate::macos_account::Password::from_text)
            .transpose()
        {
            Ok(password) => password,
            Err(error) => {
                if let Ok(mut state) = start_state.lock() {
                    state.message = error.to_string();
                }
                return;
            }
        };
        let mut state = match start_state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if state.running {
            return;
        }
        state.running = true;
        state.can_cancel = false;
        state.start_account = None;
        state.message = "Starting the development desktop. Complete or cancel macOS approval locally; there is no deadline.".into();
        drop(state);
        start_cancelled.store(false, Ordering::Relaxed);
        let state = start_state.clone();
        let cancelled = start_cancelled.clone();
        std::thread::spawn(move || {
            let name = request.name;
            let mut password = password;
            let started = desktop_start::run(
                &name,
                &cancelled,
                || {
                    password
                        .take()
                        .context("the desktop is now missing; choose Start environment again to enter its password")
                },
                |progress| {
                    if let Ok(mut state) = state.lock() {
                        state.message = progress.message;
                        state.can_cancel = progress.can_cancel;
                    }
                },
            );
            drop(password);
            let mut progress = |progress: crate::macos_setup::Progress| {
                if let Ok(mut state) = state.lock() {
                    state.message = progress.message;
                    state.can_cancel = progress.can_cancel;
                }
            };
            let cleaned = matches!(started, Ok(desktop_start::Outcome::CleanupFinished));
            let result = started.and_then(|_| {
                if cleaned {
                    return guided::continue_existing(&name, &cancelled, &mut progress);
                }
                if let Some(saved) = guided::saved()?
                    && guided::can_resume(&saved)?
                {
                    let addr = saved.addr.clone();
                    guided::run(
                        guided::Options {
                            name: &name,
                            create: None,
                            non_interactive: false,
                            launchagent: saved.launchagent,
                            addr: Some(&addr),
                            system_dialog: true,
                            cancelled: &cancelled,
                            resume: Some(saved),
                            previous: None,
                        },
                        &mut progress,
                    )
                } else {
                    guided::continue_existing(&name, &cancelled, &mut progress)
                }
            });
            if let Err(error) = &result {
                eprintln!("glass: desktop startup failed: {error:#}");
            }
            if let Ok(mut state) = state.lock() {
                state.running = false;
                state.can_cancel = true;
                state.start_account = result
                    .as_ref()
                    .err()
                    .and_then(|_| desktop_start::choice(&name).ok());
                state.message = match result {
                    Ok(()) => format!(
                        "{name} ready. Your existing MCP endpoint can run applications in its separate desktop."
                    ),
                    Err(error) if cleaned && error.is::<guided::EnvironmentMissing>() => format!(
                        "Previous temporary access cleanup finished. The desktop for {name} is not running. Choose Start environment… to start it with a fresh local password; no authentication was replayed."
                    ),
                    Err(error) => format!(
                        "Desktop startup needs attention: {error:#}. Account, password and files retained; retry only by choosing Start environment… explicitly."
                    ),
                };
            }
        });
    });
    if diagnose {
        let diagnostic_state = state.clone();
        if let Ok(mut state) = state.lock() {
            state.running = true;
            state.message = "Checking the enrolled development desktop…".into();
        }
        if let Some(name) = enrolled_name {
            std::thread::spawn(move || {
                let phase = guided::diagnostic_phase();
                if let Ok(mut state) = diagnostic_state.lock() {
                    state.running = false;
                    state.can_cancel = true;
                    state.start_account = phase
                        .as_ref()
                        .ok()
                        .filter(|phase| {
                            matches!(
                                phase,
                                crate::macos_setup::Phase::LoginRequired
                                    | crate::macos_setup::Phase::StartupCleanupRequired
                            )
                        })
                        .and_then(|_| desktop_start::choice(&name).ok());
                    state.message = match phase {
                        Ok(phase) => phase.message(&name),
                        Err(error) => format!("Desktop status needs attention: {error:#}"),
                    };
                }
            });
        }
    }
    let actions = SetupActions {
        account,
        resume_creation,
        on_resume: automatic.map(|saved| {
            Box::new(move || resume_begin(saved.name.clone(), None, Some(saved)))
                as Box<dyn FnOnce()>
        }),
        on_begin: Box::new(move |name| begin(name, None, None)),
        on_start,
        on_create: Box::new(move |request| {
            match crate::macos_account::Password::from_text(request.password) {
                Ok(password) => {
                    let resume = (|| -> anyhow::Result<_> {
                        let owned = create_pending
                            .lock()
                            .map_err(|_| anyhow::anyhow!("setup state is unavailable"))?
                            .clone();
                        let current = guided::saved()?;
                        crate::macos_setup::pending::Pending::creation_retry(
                            create_saved.as_ref(),
                            owned.as_deref(),
                            current.as_ref(),
                            &request.name,
                            request.previous_attempt.as_deref(),
                        )
                    })();
                    let resume = match resume {
                        Ok(resume) => resume,
                        Err(error) => {
                            if let Ok(mut state) = create_state.lock() {
                                state.message = format!(
                                    "{error:#}. Close this window and open setup again before starting another attempt."
                                );
                            }
                            return;
                        }
                    };
                    create(request.name, Some(password), resume);
                }
                Err(error) => {
                    if let Ok(mut state) = create_state.lock() {
                        state.message = error.to_string();
                    }
                }
            }
        }),
        on_disable: Box::new(move || {
            let mut state = match disable_state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            if state.running {
                return;
            }
            state.running = true;
            state.can_cancel = false;
            state.message = "Disabling isolation. Glass will restart its managed server; the account and its files will be kept. Authenticate locally if asked.".into();
            drop(state);
            let state = disable_state.clone();
            #[cfg(feature = "network")]
            let addr = disable_addr.clone();
            std::thread::spawn(move || {
                let result = leave_account_using(false, true);
                #[cfg(feature = "network")]
                let restart_error = result
                    .as_ref()
                    .ok()
                    .and_then(|()| restart_managed_after_disable(&addr).err());
                #[cfg(not(feature = "network"))]
                let restart_error: Option<anyhow::Error> = None;
                if let Ok(mut state) = state.lock() {
                    state.running = false;
                    state.can_cancel = true;
                    state.message = match result {
                        Ok(()) => match restart_error {
                            Some(error) => format!("Isolation disabled; the account and files were kept. Managed-server restart needs attention: {error:#}"),
                            None => "Isolation disabled; the account, files and grants were kept. Reconnect client-spawned stdio servers before shared-desktop work.".into(),
                        },
                        Err(error) => format!("Could not disable isolation: {error:#}"),
                    };
                }
            });
        }),
        poll: Box::new(move || {
            state.lock().map(|state| state.clone()).unwrap_or(SetupProgress {
            message: "Setup state is unavailable. Close this window and open application setup again.".into(), running: false, can_cancel: true, start_account: None,
        })
        }),
        can_close: Box::new(move || {
            close_state
                .lock()
                .map(|state| {
                    let allowed = !state.running || state.can_cancel;
                    if allowed {
                        close_cancelled.store(true, Ordering::Relaxed);
                    }
                    allowed
                })
                .unwrap_or(true)
        }),
    };
    let result = glass_macos::setup_window::run_setup(actions).map_err(anyhow::Error::msg);
    cancelled.store(true, Ordering::Relaxed);
    let owned_id = pending_id.lock().ok().and_then(|id| id.clone());
    if let Some(id) = owned_id {
        guided::forget(&id)?;
    } else if let Some(saved) = saved {
        match guided::environment_lease(crate::macos_setup::LeaseKind::Application) {
            Ok(_setup) => guided::forget(&saved.id)?,
            Err(error)
                if matches!(
                    error.downcast_ref::<fs4::TryLockError>(),
                    Some(fs4::TryLockError::WouldBlock)
                ) => {}
            Err(error) => return Err(error),
        }
    }
    result
}

fn register_setup(name: &str, non_interactive: bool, system_dialog: bool) -> anyhow::Result<()> {
    ensure!(
        !rustix::process::geteuid().is_root(),
        "run glass-mcp setup from your normal account; it requests elevation for registration only"
    );
    let account = account_record(name, rustix::process::getuid().as_raw(), false)
        .context("select an existing development account in Glass application setup")?;
    let unused_creation = account_creation::check_created_account(&account, false)?;
    if let Ok(existing) = read_enrollment() {
        ensure!(
            existing.owner_uid == rustix::process::getuid().as_raw(),
            "macOS account registration belongs to another owner"
        );
        ensure!(
            existing.name == name,
            "changing the enrolled account is not supported by this preview; the existing registration is retained"
        );
    }
    let reusable = if private_session::is_owner() {
        let account = read_enrollment()?;
        ensure!(
            account.name == name,
            "a different account is enrolled; uninstall its registration first"
        );
        crate::macos_worker::check_endpoint(account.owner_uid).is_ok()
            && crate::macos_worker::broker_requirement().is_ok()
    } else {
        false
    };
    let onboarding_installed = reusable && permission_helper_registered(&account)?;
    if !reusable || unused_creation || (!non_interactive && !onboarding_installed) {
        let mut arguments = vec!["macos-enroll", name];
        if !reusable && private_session::is_owner() {
            arguments.push("--repair");
        }
        if !non_interactive {
            arguments.push("--onboarding");
        }
        elevate_using(&arguments, non_interactive, system_dialog)?;
    }
    Ok(())
}

fn finish_setup(launchagent: bool, addr: Option<&str>) -> anyhow::Result<()> {
    use sha2::Digest;
    let account = read_enrollment()?;
    let lease = File::open(owner_root()?.join(ACCOUNT))?;
    fs4::FileExt::try_lock(&lease)
        .context("Glass application work started while setup was finishing")?;
    let binding = sha2::Sha256::digest(std::fs::read(owner_root()?.join(ACCOUNT))?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    crate::macos_worker::backend_transport::finish_permission_recovery(
        account.uid,
        binding,
        &lease,
    )
    .context("previous workspace permission recovery has not finished")?;
    let addr = addr.unwrap_or("127.0.0.1:7300");
    if launchagent {
        let exe = std::env::current_exe()?;
        let target = crate::setup::launch_agent_target(rustix::process::getuid().as_raw());
        let loaded = Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()?;
        let running_here = loaded.status.success()
            && crate::setup::checked_loaded_agent_pid(&String::from_utf8_lossy(&loaded.stdout))?
                == Some(std::process::id());
        if !running_here
            && let Some((label, pending)) =
                crate::setup::install_launch_agent(&exe.to_string_lossy(), addr)?
        {
            bail!("{label}: {pending}");
        }
        crate::setup::verify_private_launch_agent(addr)?;
    }
    let phase = crate::macos_worker::backend_transport::readiness_with_lease(lease)?.phase();
    ensure!(
        phase == crate::macos_setup::Phase::Ready,
        "{}",
        phase.message(&account.name)
    );
    Ok(())
}

#[cfg(feature = "network")]
fn restart_managed_after_disable(addr: &str) -> anyhow::Result<()> {
    let target = crate::setup::launch_agent_target(rustix::process::getuid().as_raw());
    let loaded = Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()?;
    if !loaded.status.success() {
        return Ok(());
    }
    let binary = std::env::current_exe()?.canonicalize()?;
    ensure!(
        loaded_job_matches(
            &String::from_utf8_lossy(&loaded.stdout),
            &[
                &binary.to_string_lossy(),
                "serve",
                "--http",
                "--menubar",
                "--addr",
                addr
            ]
        ),
        "isolation is disabled, but the managed server has different launch settings; restart that MCP connection"
    );
    crate::setup::restart_launch_agent()?;
    Ok(())
}

/// Readiness uses the native worker's grants/session, never the calling terminal's TCC state.
pub fn verify() -> anyhow::Result<()> {
    ensure!(
        private_session::configured()?,
        "no private macOS account enrolled"
    );
    let account = read_enrollment()?;
    let phase = crate::macos_worker::backend_transport::readiness()
        .with_context(|| format!("account {} is enrolled, but its private worker connection failed; open Glass application setup to check its registration", account.name))?.phase();
    ensure!(
        phase == crate::macos_setup::Phase::Ready,
        "{}",
        phase.message(&account.name)
    );
    println!(
        "glass: {} ready; apps use its private desktop through the normal MCP tools",
        account.name
    );
    Ok(())
}

fn remove_onboarding(
    account: &Enrollment,
    binary: &Path,
    root: &File,
    agents: &File,
) -> anyhow::Result<()> {
    let recorded = stored_file(root, ONBOARD_JOB)?;
    let installed = stored_file(agents, ONBOARD_JOB)?;
    if recorded.is_none() && installed.is_none() {
        return Ok(());
    }
    let expected = onboarding_job(account, binary)?;
    ensure!(
        recorded.as_ref().is_none_or(|bytes| *bytes == expected)
            && installed.as_ref().is_none_or(|bytes| *bytes == expected),
        "permission onboarding belongs to another installation; enrollment retained"
    );
    let target = format!("gui/{}/{ONBOARD_LABEL}", account.uid);
    let loaded_job = crate::launchd::inspect(&target)?;
    let loaded = loaded_job.is_some();
    ensure!(
        !loaded || installed.is_some(),
        "permission onboarding file is missing; refusing unbound cleanup"
    );
    if let Some(loaded_job) = loaded_job {
        ensure!(
            loaded_job_matches(
                &loaded_job,
                &[
                    &binary.to_string_lossy(),
                    "macos-onboarding",
                    &account.owner_uid.to_string()
                ]
            ),
            "loaded permission helper changed; registration retained"
        );
        stop_registered_worker(&target, account)?;
    }
    if installed.is_some() {
        rustix::fs::unlinkat(agents, ONBOARD_JOB, rustix::fs::AtFlags::empty())?;
        agents.sync_all()?;
    }
    if recorded.is_some() {
        rustix::fs::unlinkat(root, ONBOARD_JOB, rustix::fs::AtFlags::empty())?;
        root.sync_all()?;
    }
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    if !owner_root()?.try_exists()? {
        return Ok(());
    }
    let account = read_enrollment()?;
    ensure!(
        account.owner_uid == rustix::process::getuid().as_raw(),
        "macOS account registration belongs to another owner"
    );
    let lease = File::open(owner_root()?.join(ACCOUNT))?;
    let busy = fs4::FileExt::try_lock(&lease).is_err();
    println!(
        "macOS account: {} ({})",
        account.name,
        if !private_session::is_owner() {
            "disabled"
        } else if busy {
            "in use"
        } else {
            "idle"
        }
    );
    println!(
        "work: glass_stop in your MCP client; inspection: switch to this account; keep it logged in for reuse"
    );
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    leave_account(false)
}

/// Restore this user's shared-desktop route while preserving the normal MCP integration.
pub fn leave_account(non_interactive: bool) -> anyhow::Result<()> {
    leave_account_using(non_interactive, false)
}

fn leave_account_using(non_interactive: bool, system_dialog: bool) -> anyhow::Result<()> {
    let _setup = guided::lock_setup()?;
    guided::forget_saved()?;
    if !owner_root()?.try_exists()? {
        let owner = rustix::process::geteuid().as_raw();
        let retired = private_session::root_for(owner)
            .parent()
            .context("registration parent unavailable")?
            .join(format!(".unenroll-{owner}"));
        let parent = retired
            .parent()
            .context("registration parent unavailable")?;
        let staged = if parent.try_exists()? {
            !staged_names(&File::open(parent)?, owner)?.is_empty()
        } else {
            false
        };
        if retired.try_exists()? || staged {
            elevate_using(&["macos-unenroll"], non_interactive, system_dialog)?;
        }
        println!(
            "glass: no separate-account enrollment for this user; shared-desktop use is available"
        );
        return Ok(());
    }
    let account = read_enrollment()?;
    ensure!(
        account.owner_uid == rustix::process::getuid().as_raw(),
        "macOS account registration belongs to another owner"
    );
    let lease = File::open(owner_root()?.join(ACCOUNT))?;
    fs4::FileExt::try_lock(&lease)
        .context("stop active Glass application work before unenrolling")?;
    use sha2::Digest;
    let binding = sha2::Sha256::digest(std::fs::read(owner_root()?.join(ACCOUNT))?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    crate::macos_worker::backend_transport::finish_permission_recovery(
        account.uid,
        binding,
        &lease,
    )?;
    drop(lease);
    elevate_using(&["macos-unenroll"], non_interactive, system_dialog)?;
    ensure!(
        !private_session::configured()?,
        "macOS account enrollment remains active"
    );
    println!(
        "glass: shared-desktop use restored; the app, account, grants and MCP startup integration were retained"
    );
    println!("glass: restart existing Glass MCP servers before starting shared-desktop work");
    Ok(())
}

/// Remove only the registered agent; retain the user's account, home, grants and app data.
pub fn remove_registration() -> anyhow::Result<()> {
    let owner = root_owner()?;
    let _owner_lock = mutation_lock("owner", owner)?;
    let legacy_path = Path::new(private_session::ROOT).join(ACCOUNT);
    let mut legacy_lease = if legacy_path.try_exists()? && private_session::legacy_owner()? == owner
    {
        let file = File::open(&legacy_path)?;
        fs4::FileExt::try_lock(&file)
            .context("stop active legacy Glass work before unenrolling")?;
        Some(file)
    } else {
        None
    };
    loop {
        if !owner_root()?.try_exists()? {
            let parent_path = private_session::root_for(owner)
                .parent()
                .context("registration parent unavailable")?
                .to_path_buf();
            if parent_path.try_exists()? {
                let parent = directory(&parent_path, 0, false)?;
                cleanup_retired(&parent, &format!(".unenroll-{owner}"))?;
                for staged in staged_names(&parent, owner)? {
                    cleanup_retired(&parent, &staged)?;
                }
            }
            return Ok(());
        }
        let account = read_enrollment()?;
        let _worker_lock = mutation_lock("worker", account.uid)?;
        ensure!(
            account.owner_uid == owner,
            "account setup belongs to another user"
        );
        let root_path = owner_root()?;
        let root = directory(&root_path, 0, false)?;
        fs4::FileExt::try_lock(&root).context("account setup is already changing")?;
        let lease = if root_path == Path::new(private_session::ROOT) {
            legacy_lease
                .take()
                .context("legacy registration lease unavailable")?
        } else {
            let file = File::from(openat(
                &root,
                ACCOUNT,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            fs4::FileExt::try_lock(&file)
                .context("Glass application work is active; nothing removed")?;
            file
        };
        let _lease = lease;
        desktop_start::restore_pending(&account)?;
        let agents = directory(
            &account.home.join("Library/LaunchAgents"),
            account.uid,
            false,
        )?;
        let binary = std::env::current_exe()?.canonicalize()?;
        let legacy_owned = Path::new(private_session::ROOT)
            .join(ACCOUNT)
            .try_exists()?
            && private_session::legacy_owner()? == owner;
        let expected = job(&account, &binary)?;
        let legacy = if legacy_owned {
            legacy_job(&account, &binary)?
        } else {
            expected.clone()
        };
        let registered_file = accepts_job(&agents, &expected, &legacy)?;
        let target = format!("gui/{}/{LABEL}", account.uid);
        let loaded_job = crate::launchd::inspect(&target)?;
        let loaded = loaded_job.is_some();
        ensure!(
            registered_file || !loaded,
            "worker job is loaded but its registered file is missing; refusing unbound cleanup"
        );
        if let Some(loaded_job) = loaded_job {
            let binary = binary.to_string_lossy();
            let owner = account.owner_uid.to_string();
            ensure!(
                loaded_job_matches(&loaded_job, &[&binary, "macos-worker", &owner])
                    || (legacy_owned
                        && loaded_job_matches(&loaded_job, &[&binary, "macos-worker"])),
                "loaded worker belongs to another installation; registration retained"
            );
        }
        // A failed cleanup must leave clients on the disabled private route.
        if !same_file(&root, "disabled", b"Glass account registration removed\n")? {
            create_file(&root, "disabled", b"Glass account registration removed\n")?;
        }
        remove_onboarding(&account, &binary, &root, &agents)?;
        unregister_worker(&root, &account, || {
            if loaded {
                stop_registered_worker(&target, &account)?;
            }
            Ok(())
        })?;
        if registered_file {
            rustix::fs::unlinkat(&agents, JOB, rustix::fs::AtFlags::empty())?;
            agents.sync_all()?;
        }
        if root_path == Path::new(private_session::ROOT) {
            for name in ["worker.requirement", "disabled"] {
                match rustix::fs::unlinkat(&root, name, rustix::fs::AtFlags::empty()) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            root.sync_all()?;
            rustix::fs::unlinkat(&root, ACCOUNT, rustix::fs::AtFlags::empty())?;
            root.sync_all()?;
        } else {
            finish_removal(&root_path, &root)?;
        }
        println!(
            "glass: worker registration removed; account {}, its home and permissions retained",
            account.name
        );
    }
}

pub(crate) fn checks() -> Vec<glass_core::Check> {
    use glass_core::{Check, CheckStatus};
    match read_enrollment() {
        Ok(account) if private_session::is_owner() => vec![
            Check::new("private account", CheckStatus::Ok, format!("{} enrolled; real login and grants are checked at worker start", account.name)),
            match crate::macos_worker::check_endpoint(account.owner_uid) {
                Ok(()) => Check::new("worker endpoint", CheckStatus::Ok, "registered; readiness is checked without a desktop fallback at glass_start"),
                Err(error) => Check::new("worker endpoint", CheckStatus::Fail, error.to_string())
                    .with_remedy(format!("run glass-mcp setup --account {} to check registration and restore worker socket access", account.name)),
            },
            match guided::diagnostic_phase() {
                Ok(crate::macos_setup::Phase::Ready) => Check::new("worker readiness", CheckStatus::Ok, "authenticated worker has a private graphical login, Accessibility and Screen Recording"),
                Ok(crate::macos_setup::Phase::Busy) => Check::new("worker readiness", CheckStatus::Warn, "environment is in use; readiness probe skipped without interrupting it"),
                Ok(phase) => Check::new("worker readiness", CheckStatus::Fail, phase.message(&account.name)).with_remedy("open Glass → Application setup… to resume setup or repair"),
                Err(error) => Check::new("worker readiness", CheckStatus::Fail, error.to_string()).with_remedy("open Glass → Application setup…"),
            },
        ],
        Ok(_) => vec![Check::new("private account", CheckStatus::Fail, "registration disabled or belongs to another owner")],
        Err(error) => vec![Check::new("private account", CheckStatus::Fail, error.to_string()).with_remedy("run glass-mcp setup --account ACCOUNT to repair registration")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn loaded_job_binding_rejects_another_owner_program_or_argument_order() {
        let output = "gui/502/example = {\n program = /Applications/Glass.app/Contents/MacOS/glass-mcp\n arguments = {\n /Applications/Glass.app/Contents/MacOS/glass-mcp\n macos-onboarding\n 501\n }\n environment = {\n pid = 900\n }\n}";
        let expected = [
            "/Applications/Glass.app/Contents/MacOS/glass-mcp",
            "macos-onboarding",
            "501",
        ];
        assert!(loaded_job_matches(output, &expected));
        assert!(loaded_job_matches(
            &output.replace(
                " environment = {",
                " environment = {\n \"reqs\" => {\n pid = 901\n }"
            ),
            &expected
        ));
        assert!(!loaded_job_matches(
            &output.replace("\n 501\n", "\n 503\n"),
            &expected
        ));
        assert!(!loaded_job_matches(
            &output.replace(
                "program = /Applications/Glass.app",
                "program = /Applications/Other.app"
            ),
            &expected
        ));
        assert!(!loaded_job_matches(
            &output.replace("macos-onboarding\n 501", "501\n macos-onboarding"),
            &expected
        ));
        assert!(!loaded_job_matches(output.trim_end_matches('}'), &expected));
        assert!(!loaded_job_matches(
            &output.replace(" environment = {", " program = /other\n environment = {"),
            &expected
        ));
    }

    #[test]
    fn permission_job_uses_the_same_glass_binary_and_an_explicit_owner_role() {
        let account = Enrollment {
            version: 1,
            uid: 502,
            gid: 20,
            record_uuid: "test".into(),
            name: "dev".into(),
            home: "/Users/dev".into(),
            owner_uid: 501,
        };
        let bytes = onboarding_job(
            &account,
            Path::new("/Applications/GlassMcp.app/Contents/MacOS/glass-mcp"),
        )
        .unwrap();
        let job: plist::Dictionary = plist::from_bytes(&bytes).unwrap();
        let args = job["ProgramArguments"].as_array().unwrap();
        assert_eq!(
            args.iter()
                .map(|value| value.as_string().unwrap())
                .collect::<Vec<_>>(),
            [
                "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp",
                "macos-onboarding",
                "501"
            ]
        );
        assert_eq!(job["RunAtLoad"].as_boolean(), Some(true));
        assert_eq!(job["KeepAlive"].as_boolean(), Some(false));
        assert_eq!(job["LimitLoadToSessionType"].as_string(), Some("Aqua"));
        assert!(!job.contains_key("Sockets"));
    }

    #[test]
    fn retired_registration_accepts_only_the_protected_permission_job_record() {
        let directory = tempfile::tempdir().unwrap();
        let root = File::open(directory.path()).unwrap();
        std::fs::write(directory.path().join(ONBOARD_JOB), b"job").unwrap();
        std::fs::set_permissions(
            directory.path().join(ONBOARD_JOB),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(registration_files(&root).unwrap(), [ONBOARD_JOB]);
        std::fs::write(directory.path().join("unrelated.plist"), b"keep").unwrap();
        assert!(registration_files(&root).is_err());
    }

    fn account() -> Enrollment {
        Enrollment {
            version: 1,
            uid: 504,
            gid: 20,
            record_uuid: "00000000-0000-0000-0000-000000000004".into(),
            name: "development".into(),
            home: "/Users/development".into(),
            owner_uid: 501,
        }
    }

    #[test]
    fn interrupted_marker_write_does_not_permanently_block_unenrollment() {
        let registry = tempfile::tempdir().unwrap();
        let owner = registry.path().join("501");
        std::fs::create_dir(&owner).unwrap();
        std::fs::write(owner.join(ACCOUNT), "complete registration").unwrap();
        std::fs::write(
            owner.join(format!(".setup-{}", "01".repeat(16))),
            "interrupted marker",
        )
        .unwrap();
        retire_registration(
            &File::open(registry.path()).unwrap(),
            "501",
            &File::open(&owner).unwrap(),
        )
        .unwrap();
        assert!(!owner.exists());
        assert!(!registry.path().join(".unenroll-501").exists());
    }

    #[test]
    fn setup_recovery_names_are_canonical_and_bound_to_the_owner() {
        let registry = tempfile::tempdir().unwrap();
        let ours = format!(".setup-501-{}", "ab".repeat(16));
        for name in [
            &ours,
            &format!(".setup-502-{}", "ab".repeat(16)),
            ".setup-501-unknown",
        ] {
            std::fs::create_dir(registry.path().join(name)).unwrap();
        }
        assert_eq!(
            staged_names(&File::open(registry.path()).unwrap(), 501).unwrap(),
            vec![ours]
        );
        assert!(!setup_suffix(".setup-../unrelated", ".setup-"));
    }

    #[test]
    fn failed_registration_preparation_never_publishes_an_incomplete_owner_route() {
        let registry = tempfile::tempdir().unwrap();
        let parent = File::open(registry.path()).unwrap();
        let result = stage_registration(&parent, "501", |root| {
            let mut file = File::from(openat(
                root,
                ACCOUNT,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                Mode::from_raw_mode(0o600),
            )?);
            file.write_all(b"partial record")?;
            bail!("injected preparation failure")
        });
        assert!(result.is_err());
        assert!(!registry.path().join("501").exists());
        assert_eq!(std::fs::read_dir(registry.path()).unwrap().count(), 0);
    }

    #[test]
    fn published_registration_remains_readable_after_restrictive_creation() {
        let registry = tempfile::tempdir().unwrap();
        let parent = File::open(registry.path()).unwrap();
        let root = stage_registration(&parent, "501", |root| {
            // A caller with umask 077 creates the staging directory this way.
            rustix::fs::fchmod(root, Mode::from_raw_mode(0o700))?;
            Ok(())
        })
        .unwrap();
        assert_eq!(root.metadata().unwrap().mode() & 0o777, 0o755);
        assert_eq!(
            std::fs::metadata(registry.path().join("501"))
                .unwrap()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn registration_permissions_repair_an_inaccessible_existing_registration() {
        let registry = tempfile::tempdir().unwrap();
        let root = File::open(registry.path()).unwrap();
        rustix::fs::fchmod(&root, Mode::from_raw_mode(0o700)).unwrap();
        std::fs::write(registry.path().join(ACCOUNT), b"retained account").unwrap();
        registration_permissions(&root).unwrap();
        assert_eq!(root.metadata().unwrap().mode() & 0o777, 0o755);
        assert_eq!(
            std::fs::read(registry.path().join(ACCOUNT)).unwrap(),
            b"retained account"
        );
    }

    #[test]
    fn complete_registration_is_published_without_replacing_an_existing_route() {
        let registry = tempfile::tempdir().unwrap();
        let parent = File::open(registry.path()).unwrap();
        let staged = stage_registration(&parent, "501", |root| {
            let mut file = File::from(openat(
                root,
                ACCOUNT,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                Mode::from_raw_mode(0o600),
            )?);
            file.write_all(b"complete record")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            std::fs::read(registry.path().join("501").join(ACCOUNT)).unwrap(),
            b"complete record"
        );
        assert!(stage_registration(&parent, "501", |_| Ok(())).is_err());
        assert_eq!(
            std::fs::read(registry.path().join("501").join(ACCOUNT)).unwrap(),
            b"complete record"
        );
        drop(staged);
    }

    #[test]
    fn public_removal_restores_only_the_selected_owner_and_preserves_startup_files() {
        let registry = tempfile::tempdir().unwrap();
        let owner = registry.path().join("501");
        let other = registry.path().join("502");
        let startup = registry.path().join("owner-http.plist");
        std::fs::create_dir(&owner).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(&startup, "retained HTTP configuration").unwrap();
        for name in [ACCOUNT, "worker.requirement", "disabled"] {
            std::fs::write(owner.join(name), "owned registration metadata").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(owner.join(name), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        retire_registration(
            &File::open(registry.path()).unwrap(),
            "501",
            &File::open(&owner).unwrap(),
        )
        .unwrap();
        assert!(!owner.exists());
        assert!(other.is_dir());
        assert_eq!(
            std::fs::read_to_string(startup).unwrap(),
            "retained HTTP configuration"
        );
        assert!(!registry.path().join(".unenroll-501").exists());
    }

    #[test]
    fn unexpected_enrollment_contents_keep_the_owners_route_closed() {
        let registry = tempfile::tempdir().unwrap();
        let owner = registry.path().join("501");
        std::fs::create_dir(&owner).unwrap();
        std::fs::write(owner.join("unrelated"), "do not delete").unwrap();
        assert!(
            retire_registration(
                &File::open(registry.path()).unwrap(),
                "501",
                &File::open(&owner).unwrap()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(owner.join("unrelated")).unwrap(),
            "do not delete"
        );
    }

    #[test]
    fn completed_route_transition_metadata_cleanup_is_retryable() {
        let registry = tempfile::tempdir().unwrap();
        let retired = registry.path().join(".unenroll-501");
        std::fs::create_dir(&retired).unwrap();
        std::fs::write(retired.join(ACCOUNT), "remaining owned metadata").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            retired.join(ACCOUNT),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let parent = File::open(registry.path()).unwrap();
        cleanup_retired(&parent, ".unenroll-501").unwrap();
        cleanup_retired(&parent, ".unenroll-501").unwrap();
        assert!(!retired.exists());
    }

    #[test]
    fn worker_jobs_bind_the_owner_namespace_and_command_argument() {
        let account = Enrollment {
            version: 1,
            uid: 502,
            gid: 20,
            record_uuid: "test-record".into(),
            name: "test-worker".into(),
            home: PathBuf::from("/Users/test-worker"),
            owner_uid: 501,
        };
        let value: plist::Value = plist::from_bytes(
            &job(
                &account,
                Path::new("/Applications/GlassMcp.app/Contents/MacOS/glass-mcp"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            value.as_dictionary().unwrap()["ProgramArguments"]
                .as_array()
                .unwrap()[2]
                .as_string(),
            Some("501")
        );
        let sockets = value.as_dictionary().unwrap()["Sockets"]
            .as_dictionary()
            .unwrap();
        assert_eq!(
            sockets["Worker"].as_dictionary().unwrap()["SockPathName"].as_string(),
            Some("/Library/Application Support/Glass/accounts/501/worker.sock")
        );
    }

    #[test]
    fn worker_job_uses_only_the_native_backend_service() {
        let bytes = job(
            &account(),
            Path::new("/Applications/Glass & Tools.app/Contents/MacOS/glass-mcp"),
        )
        .unwrap();
        let job: plist::Dictionary = plist::from_bytes(&bytes).unwrap();
        let args = job["ProgramArguments"].as_array().unwrap();
        assert_eq!(args.len(), 3);
        assert_eq!(args[1].as_string(), Some("macos-worker"));
        assert_eq!(job["RunAtLoad"].as_boolean(), Some(false));
        let sockets = job["Sockets"].as_dictionary().unwrap();
        let socket = sockets["Worker"].as_dictionary().unwrap();
        assert!(!socket.contains_key("SockPathOwner"));
        assert_eq!(socket["SockPathMode"].as_unsigned_integer(), Some(0o600));
        assert!(!job.contains_key("UserName"));
        assert!(!job.contains_key("EnvironmentVariables"));
    }

    fn socket_directory() -> (tempfile::TempDir, File, std::os::unix::net::UnixListener) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("worker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let parent = File::open(root.path()).unwrap();
        (root, parent, listener)
    }

    #[test]
    fn reenrollment_allows_worker_checkin_before_waiting_for_its_socket() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let parent = File::open(root.path()).unwrap();
        let disabled = root.path().join("disabled");
        std::fs::write(&disabled, b"Glass account registration removed\n").unwrap();
        let mut account = account();
        account.owner_uid = rustix::process::getuid().as_raw();
        let socket = root.path().join("worker.sock");
        let mut listener = None;
        enable_worker(&parent, &account, || {
            ensure!(
                !disabled.try_exists()?,
                "worker refuses disabled enrollment"
            );
            listener = Some(std::os::unix::net::UnixListener::bind(&socket)?);
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            Ok(true)
        })
        .unwrap();
        assert_eq!(
            std::fs::symlink_metadata(socket).unwrap().uid(),
            account.owner_uid
        );
    }

    #[test]
    fn reenrollment_without_an_aqua_login_keeps_registration_pending() {
        let root = tempfile::tempdir().unwrap();
        let parent = File::open(root.path()).unwrap();
        std::fs::write(root.path().join("disabled"), b"removed").unwrap();
        enable_worker(&parent, &account(), || Ok(false)).unwrap();
        assert!(!root.path().join("worker.sock").exists());
    }

    #[test]
    fn reenrollment_refuses_an_unremovable_marker_before_activating_a_worker() {
        let root = tempfile::tempdir().unwrap();
        let parent = File::open(root.path()).unwrap();
        std::fs::create_dir(root.path().join("disabled")).unwrap();
        let mut activated = false;
        let result = enable_worker(&parent, &account(), || {
            activated = true;
            Ok(false)
        });
        assert!(result.is_err());
        assert!(!activated);
    }

    #[test]
    fn owner_socket_normalization_is_idempotent_and_preserves_its_inode() {
        let (root, parent, _listener) = socket_directory();
        let path = root.path().join("worker.sock");
        let before = std::fs::symlink_metadata(&path).unwrap();
        let owner = rustix::process::getuid().as_raw();
        normalize_socket(&parent, owner, owner + 1).unwrap();
        normalize_socket(&parent, owner, owner + 1).unwrap();
        let after = std::fs::symlink_metadata(path).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(after.uid(), owner);
        assert_eq!(after.mode() & 0o777, 0o600);
    }

    #[test]
    fn socket_normalization_refuses_broader_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (root, parent, _listener) = socket_directory();
        let path = root.path().join("worker.sock");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let owner = rustix::process::getuid().as_raw();
        assert!(normalize_socket(&parent, owner, owner + 1).is_err());
        assert_eq!(
            std::fs::symlink_metadata(path).unwrap().mode() & 0o777,
            0o666
        );
    }

    #[test]
    fn socket_normalization_leaves_a_regular_file_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("worker.sock");
        std::fs::write(&path, b"unrelated").unwrap();
        let parent = File::open(root.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        assert!(normalize_socket(&parent, owner, owner + 1).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"unrelated");
    }

    #[test]
    fn socket_normalization_does_not_follow_a_link() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("unrelated");
        std::fs::write(&target, b"preserved").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("worker.sock")).unwrap();
        let parent = File::open(root.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        assert!(normalize_socket(&parent, owner, owner + 1).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"preserved");
    }

    #[test]
    fn removed_listener_socket_can_be_cleaned_before_registering_again() {
        let (root, parent, listener) = socket_directory();
        let path = root.path().join("worker.sock");
        drop(listener);
        assert!(std::os::unix::net::UnixStream::connect(&path).is_err());
        let owner = rustix::process::getuid().as_raw();
        remove_socket(&parent, owner, owner + 1).unwrap();
        let _new_listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    }

    #[test]
    fn failed_unregistration_preserves_socket_for_cleanup_retry() {
        let (root, parent, listener) = socket_directory();
        let socket = root.path().join("worker.sock");
        let mut account = account();
        account.owner_uid = rustix::process::getuid().as_raw();
        let error = unregister_worker(&parent, &account, || anyhow::bail!("bootout refused"));
        assert!(error.is_err());
        assert!(socket.exists());
        unregister_worker(&parent, &account, || {
            drop(listener);
            Ok(())
        })
        .unwrap();
        assert!(!socket.exists());
    }

    #[test]
    fn socket_removal_accepts_an_already_absent_node() {
        let root = tempfile::tempdir().unwrap();
        let parent = File::open(root.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        remove_socket(&parent, owner, owner + 1).unwrap();
    }

    #[test]
    fn socket_removal_preserves_a_regular_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("worker.sock");
        std::fs::write(&path, b"unrelated").unwrap();
        let parent = File::open(root.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        assert!(remove_socket(&parent, owner, owner + 1).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"unrelated");
    }

    #[test]
    fn socket_removal_does_not_follow_a_link() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("unrelated");
        std::fs::write(&target, b"preserved").unwrap();
        let path = root.path().join("worker.sock");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let parent = File::open(root.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        assert!(remove_socket(&parent, owner, owner + 1).is_err());
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"preserved");
    }

    #[test]
    fn socket_removal_preserves_unexpected_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (root, parent, _listener) = socket_directory();
        let path = root.path().join("worker.sock");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let owner = rustix::process::getuid().as_raw();
        assert!(remove_socket(&parent, owner, owner + 1).is_err());
        assert_eq!(
            std::fs::symlink_metadata(path).unwrap().mode() & 0o777,
            0o666
        );
    }

    #[test]
    fn setup_write_refuses_replacement_and_leaves_no_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let parent = File::open(root.path()).unwrap();
        create_file(&parent, "record", b"complete").unwrap();
        assert!(create_file(&parent, "record", b"replacement").is_err());
        assert_eq!(
            std::fs::read(root.path().join("record")).unwrap(),
            b"complete"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn setup_write_does_not_follow_a_replaced_record() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("unrelated"), b"preserved").unwrap();
        std::os::unix::fs::symlink("unrelated", root.path().join("record")).unwrap();
        let parent = File::open(root.path()).unwrap();
        assert!(create_file(&parent, "record", b"new").is_err());
        assert_eq!(
            std::fs::read(root.path().join("unrelated")).unwrap(),
            b"preserved"
        );
    }

    #[test]
    fn enrollment_rejects_unrecognized_fields() {
        let mut bytes = Vec::new();
        plist::to_writer_xml(&mut bytes, &account()).unwrap();
        let mut value: plist::Dictionary = plist::from_bytes(&bytes).unwrap();
        value.insert("password".into(), "must not be stored".into());
        let mut bytes = Vec::new();
        plist::to_writer_xml(&mut bytes, &value).unwrap();
        assert!(plist::from_bytes::<Enrollment>(&bytes).is_err());
    }
}
