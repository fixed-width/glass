//! One-time account enrollment; per-app work remains behind the normal backend factory.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};
use glass_core::Deadline;
use glass_macos::private_session;
use rustix::fs::{Mode, OFlags, open, openat};
use serde::{Deserialize, Serialize};

const LABEL: &str = "tech.fixedwidth.glass.worker";
const ACCOUNT: &str = "account.plist";
const JOB: &str = "tech.fixedwidth.glass.worker.plist";

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
    let bytes = checked(&mut command)?;
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

fn read_enrollment() -> anyhow::Result<Enrollment> {
    let root = directory(Path::new(private_session::ROOT), 0, false)?;
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
        format!("{}/worker.sock", private_session::ROOT).into(),
    );
    worker.insert("SockPathMode".into(), 0o600.into());
    let mut sockets = plist::Dictionary::new();
    sockets.insert("Worker".into(), worker.into());
    value.insert("Sockets".into(), sockets.into());
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &value)?;
    Ok(bytes)
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

fn same_file(parent: &File, name: &str, bytes: &[u8]) -> anyhow::Result<bool> {
    let fd = match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        result => result?,
    };
    let file = File::from(fd);
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
    let mut actual = Vec::new();
    file.take(65537).read_to_end(&mut actual)?;
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

/// Authenticated installer entry; never creates/deletes accounts or changes their credentials.
pub fn enroll(name: &str) -> anyhow::Result<()> {
    let owner = root_owner()?;
    let account = account_record(name, owner, true)?;
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
    directory(&account.home, account.uid, false).context("validating the selected account home")?;
    let root = directory(Path::new(private_session::ROOT), 0, true)?;
    let agents = directory(
        &account.home.join("Library/LaunchAgents"),
        account.uid,
        true,
    )?;
    let requirement = private_session::signing_requirement()?;
    if !same_file(&root, "worker.requirement", &requirement)? {
        create_file(&root, "worker.requirement", &requirement)?;
    }

    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &account)?;
    if !same_file(&root, ACCOUNT, &bytes)? {
        create_file(&root, ACCOUNT, &bytes)?;
    }
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
    enable_worker(&root, &account, || {
        let mut registered = Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()?
            .status
            .success();
        if !registered {
            // launchd leaves its socket node behind when a registration is removed.
            remove_socket(&root, account.owner_uid, account.uid)?;
            // A real login loads this persistent agent too; absence is a pending setup step.
            let result = Command::new("/bin/launchctl")
                .arg("bootstrap")
                .arg(&domain)
                .arg(account.home.join("Library/LaunchAgents").join(JOB))
                .output()?;
            if !result.status.success() {
                eprintln!(
                    "glass: registration saved; log into the selected account once to load its worker"
                );
            }
            registered = result.status.success();
        }
        Ok(registered)
    })?;
    println!(
        "glass: enrolled {}; account, home and login credentials remain yours",
        account.name
    );
    Ok(())
}

fn elevate(arguments: &[&str], non_interactive: bool) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let status = if non_interactive || std::io::stdin().is_terminal() {
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
        Command::new("/usr/bin/osascript")
            .args(["-e", &expression])
            .status()?
    };
    ensure!(
        status.success(),
        "macOS account setup did not complete; existing account and grants were retained"
    );
    Ok(())
}

pub fn setup(
    name: &str,
    non_interactive: bool,
    launchagent: bool,
    addr: Option<&str>,
) -> anyhow::Result<()> {
    ensure!(
        !rustix::process::geteuid().is_root(),
        "run glass-mcp setup from your normal account; it requests elevation for registration only"
    );
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
    if !reusable {
        elevate(&["macos-enroll", name], non_interactive)?;
    }

    let addr = addr.unwrap_or("127.0.0.1:7300");
    if launchagent {
        let exe = std::env::current_exe()?;
        if let Some((label, pending)) =
            crate::setup::install_launch_agent(&exe.to_string_lossy(), addr)?
        {
            bail!("{label}: {pending}");
        }
        crate::setup::verify_private_launch_agent(addr)?;
    }
    verify()
}

/// Readiness uses the native worker's grants/session, never the calling terminal's TCC state.
pub fn verify() -> anyhow::Result<()> {
    ensure!(
        private_session::configured()?,
        "no private macOS account enrolled"
    );
    let account = read_enrollment()?;
    let connection = crate::macos_worker::backend_transport::NativeConnection::connect()
        .with_context(|| format!("account {} is enrolled, but its private worker connection failed; run glass-mcp setup --account {} to check its registration", account.name, account.name))?;
    let mut backend = crate::remote_backend::backend(connection)?;
    let input = backend.platform.input_capabilities();
    backend.platform.stop_app_by(Deadline::from_millis(8000))?;
    ensure!(
        input.click.support.status == glass_core::Support::Supported
            && input.text.support.status == glass_core::Support::Supported,
        "enrollment saved; log into {}, open GlassMcp.app and enable its Accessibility and Screen Recording grants, then switch back without logging it out",
        account.name
    );
    println!(
        "glass: {} ready; apps use its private desktop through the normal MCP tools",
        account.name
    );
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    if !Path::new(private_session::ROOT).exists() {
        return Ok(());
    }
    let account = read_enrollment()?;
    ensure!(
        account.owner_uid == rustix::process::getuid().as_raw(),
        "macOS account registration belongs to another owner"
    );
    let lease = File::open(Path::new(private_session::ROOT).join(ACCOUNT))?;
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
    if !Path::new(private_session::ROOT).exists() {
        return Ok(());
    }
    let account = read_enrollment()?;
    ensure!(
        account.owner_uid == rustix::process::getuid().as_raw(),
        "macOS account registration belongs to another owner"
    );
    let lease = File::open(Path::new(private_session::ROOT).join(ACCOUNT))?;
    fs4::FileExt::try_lock(&lease)
        .context("stop active Glass application work before uninstalling")?;
    use sha2::Digest;
    let binding = sha2::Sha256::digest(std::fs::read(
        Path::new(private_session::ROOT).join(ACCOUNT),
    )?)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
    let mut journal = crate::permission_journal::PermissionJournal::open(
        &crate::permission_journal::PermissionJournal::root()?,
        account.uid,
        binding,
        None,
        None,
    )?;
    journal.finish(Deadline::from_millis(8000))?;
    drop(journal);
    drop(lease);
    elevate(&["macos-unenroll"], false)
}

/// Remove only the registered agent; retain the user's account, home, grants and app data.
pub fn unenroll() -> anyhow::Result<()> {
    let owner = root_owner()?;
    let account = read_enrollment()?;
    ensure!(
        account.owner_uid == owner,
        "account setup belongs to another user"
    );
    let root = directory(Path::new(private_session::ROOT), 0, false)?;
    let lease = File::from(openat(
        &root,
        ACCOUNT,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    fs4::FileExt::try_lock(&lease).context("Glass application work is active; nothing removed")?;
    let agents = directory(
        &account.home.join("Library/LaunchAgents"),
        account.uid,
        false,
    )?;
    let expected = job(&account, &std::env::current_exe()?.canonicalize()?)?;
    let registered_file = same_file(&agents, JOB, &expected)?;
    let target = format!("gui/{}/{LABEL}", account.uid);
    let loaded = Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()?
        .status
        .success();
    ensure!(
        registered_file || !loaded,
        "worker job is loaded but its registered file is missing; refusing unbound cleanup"
    );
    // A failed cleanup must leave clients on the disabled private route.
    if !same_file(&root, "disabled", b"Glass account registration removed\n")? {
        create_file(&root, "disabled", b"Glass account registration removed\n")?;
    }
    unregister_worker(&root, &account, || {
        if loaded {
            checked(Command::new("/bin/launchctl").args(["bootout", &target]))?;
        }
        Ok(())
    })?;
    if registered_file {
        rustix::fs::unlinkat(&agents, JOB, rustix::fs::AtFlags::empty())?;
        agents.sync_all()?;
    }
    println!(
        "glass: worker registration removed; account {}, its home and permissions retained",
        account.name
    );
    Ok(())
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
        ],
        Ok(_) => vec![Check::new("private account", CheckStatus::Fail, "registration disabled or belongs to another owner")],
        Err(error) => vec![Check::new("private account", CheckStatus::Fail, error.to_string()).with_remedy("run glass-mcp setup --account ACCOUNT to repair registration")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn worker_job_uses_only_the_native_backend_service() {
        let bytes = job(
            &account(),
            Path::new("/Applications/Glass & Tools.app/Contents/MacOS/glass-mcp"),
        )
        .unwrap();
        let job: plist::Dictionary = plist::from_bytes(&bytes).unwrap();
        let args = job["ProgramArguments"].as_array().unwrap();
        assert_eq!(args.len(), 2);
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
