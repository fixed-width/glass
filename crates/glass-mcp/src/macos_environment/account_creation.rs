//! Transient privileged account preparation, using authenticated local password transfer.

use super::*;
use crate::macos_account::{ExistingAccount, Password, Receipt, validate_name};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;
use zeroize::Zeroizing;

struct Endpoint {
    path: PathBuf,
    retained: File,
    listener: UnixListener,
}

impl Endpoint {
    fn new(owner: u32) -> anyhow::Result<Self> {
        let parent = std::fs::symlink_metadata("/private/tmp")?;
        ensure!(
            parent.is_dir() && parent.uid() == 0 && parent.mode() & 0o1000 != 0,
            "account preparation needs root-owned sticky temporary storage"
        );
        let path = Path::new("/private/tmp")
            .join(format!("glass-account-{:032x}", rand::random::<u128>()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        let retained = File::from(open(
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        ensure!(
            retained.metadata()?.uid() == owner,
            "account setup directory owner changed"
        );
        glass_macos::workspace_access::require_owner_private(&retained)?;
        let listener = match UnixListener::bind(path.join("password.sock")) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir(&path);
                return Err(error.into());
            }
        };
        let endpoint = Self {
            path,
            retained,
            listener,
        };
        std::fs::set_permissions(endpoint.socket(), std::fs::Permissions::from_mode(0o600))?;
        endpoint.listener.set_nonblocking(true)?;
        Ok(endpoint)
    }

    fn socket(&self) -> PathBuf {
        self.path.join("password.sock")
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(
            &self.retained,
            "password.sock",
            rustix::fs::AtFlags::empty(),
        );
        if let Ok(current) = std::fs::symlink_metadata(&self.path)
            && let Ok(original) = self.retained.metadata()
            && current.dev() == original.dev()
            && current.ino() == original.ino()
        {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

fn send(stream: &mut UnixStream, bytes: &[u8]) -> anyhow::Result<()> {
    ensure!(
        bytes.len() <= 8192,
        "account setup response exceeded its bound"
    );
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(bytes)?;
    Ok(())
}

fn receive(stream: &mut UnixStream, maximum: usize) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        length <= maximum,
        "account setup message exceeded its bound"
    );
    let mut bytes = Zeroizing::new(vec![0; length]);
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(super) fn create(name: &str, password: &Password, system_dialog: bool) -> anyhow::Result<()> {
    validate_name(name)?;
    let owner = rustix::process::getuid().as_raw();
    ensure!(
        owner >= 501 && rustix::process::geteuid().as_raw() == owner,
        "create the development account from your normal account"
    );
    if owner_root()?.join(ACCOUNT).try_exists()? {
        ensure!(
            read_enrollment()?.name == name,
            "a different development account is enrolled; disable it before creating another"
        );
    }
    let creation_root = Path::new(private_session::ROOT).join("account-creation");
    if !creation_root.join(format!("{name}.plist")).try_exists()? {
        glass_macos::account_setup::require_unused_name(name).map_err(anyhow::Error::msg)?;
    }
    let requirement = private_session::signing_requirement()?;
    let endpoint = Endpoint::new(owner)?;
    let socket = endpoint.socket();
    let path = socket.to_str().context("account endpoint is not UTF-8")?;
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let authorization = scope
            .spawn(|| elevate_using(&["macos-prepare-account", name, path], false, system_dialog));
        // Authentication is untimed; the child connects only after local macOS approval.
        let mut reported_refusal = false;
        let mut stream = loop {
            match endpoint.listener.accept() {
                Ok((stream, _)) => {
                    match private_session::authenticate_setup_root(&stream, &requirement, owner) {
                        Ok(()) => break stream,
                        Err(error) if !reported_refusal => {
                            eprintln!(
                                "glass: refused privileged account setup connection: {error}"
                            );
                            reported_refusal = true;
                        }
                        Err(_) => {}
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if authorization.is_finished() {
                        authorization
                            .join()
                            .map_err(|_| anyhow::anyhow!("account authorization task stopped"))??;
                        bail!(
                            "privileged account preparation exited before its verified connection"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let hello: (u32, String) = serde_json::from_slice(&receive(&mut stream, 1024)?)?;
        ensure!(
            hello == (owner, name.to_owned()),
            "account preparation request does not match this setup"
        );
        send(&mut stream, password.bytes())?;
        // Native account operations can themselves wait for local OS authorization.
        stream.set_read_timeout(None)?;
        let result: Result<(), String> = serde_json::from_slice(&receive(&mut stream, 8192)?)?;
        let authorized = authorization
            .join()
            .map_err(|_| anyhow::anyhow!("account authorization task stopped"))?;
        result.map_err(anyhow::Error::msg)?;
        authorized
    })
}

fn connect(path: &Path, owner: u32) -> anyhow::Result<UnixStream> {
    use std::os::unix::fs::FileTypeExt;
    let parent = path.parent().context("account endpoint has no directory")?;
    ensure!(
        parent.parent() == Some(Path::new("/private/tmp"))
            && path.file_name().is_some_and(|name| name == "password.sock")
            && parent
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| setup_suffix(name, "glass-account-")),
        "invalid account endpoint"
    );
    let directory = File::from(open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let metadata = directory.metadata()?;
    ensure!(
        metadata.uid() == owner && metadata.mode() & 0o777 == 0o700,
        "account endpoint directory is not private"
    );
    private_session::require_write_protected(&directory, owner)?;
    let socket = std::fs::symlink_metadata(path)?;
    ensure!(
        socket.file_type().is_socket() && socket.uid() == owner && socket.mode() & 0o777 == 0o600,
        "account endpoint was replaced"
    );
    let stream = UnixStream::connect(path)?;
    private_session::authenticate_peer(&stream, &private_session::signing_requirement()?, owner)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(stream)
}

fn write_receipt(root: &File, key: &str, receipt: &Receipt) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, receipt)?;
    let temporary = format!(".creation-{:032x}", rand::random::<u128>());
    let mut file = File::from(openat(
        root,
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let result = (|| {
        private_session::require_write_protected(&file, 0)?;
        file.write_all(&bytes)?;
        rustix::fs::fchmod(&file, Mode::from_raw_mode(0o644))?;
        file.sync_all()?;
        rustix::fs::renameat(root, &temporary, root, key)?;
        root.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(root, &temporary, rustix::fs::AtFlags::empty());
    }
    result
}

fn receipt(owner: u32, name: &str, root: &File) -> anyhow::Result<Receipt> {
    let key = format!("{name}.plist");
    if let Some(bytes) = stored_file(root, &key)? {
        let receipt: Receipt = plist::from_bytes(&bytes)?;
        receipt.validate(owner, name)?;
        let current = account_record(name, owner, false);
        if let Ok(current) = current {
            receipt.verify_record(current.uid, &current.record_uuid, &current.home)?;
        } else {
            let home_exists = match std::fs::symlink_metadata(Path::new("/Users").join(name)) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            receipt.verify_missing(home_exists)?;
        }
        return Ok(receipt);
    }
    ensure!(
        matches!(std::fs::symlink_metadata(Path::new("/Users").join(name)),Err(error) if error.kind() == std::io::ErrorKind::NotFound),
        "new account home already exists; select an existing account or another name"
    );
    glass_macos::account_setup::require_unused_name(name).map_err(anyhow::Error::msg)?;
    let uid = crate::account_uid::available()?;
    let random = format!("{:032x}", rand::random::<u128>());
    let uuid = format!(
        "{}-{}-4{}-8{}-{}",
        &random[..8],
        &random[8..12],
        &random[13..16],
        &random[17..20],
        &random[20..]
    );
    let receipt = Receipt {
        version: 1,
        owner,
        name: name.into(),
        uid,
        uuid,
        created: false,
        password_set: false,
    };
    receipt.validate(owner, name)?;
    write_receipt(root, &key, &receipt)?;
    Ok(receipt)
}

pub(super) fn check_created_account(
    account: &Enrollment,
    retire_unused: bool,
) -> anyhow::Result<bool> {
    let path = Path::new(private_session::ROOT).join("account-creation");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let root = directory(&path, 0, false)?;
    let Some(bytes) = stored_file(&root, &format!("{}.plist", account.name))? else {
        return Ok(false);
    };
    let receipt: Receipt = plist::from_bytes(&bytes)?;
    receipt.validate(receipt.owner, &account.name)?;
    match receipt.existing_selection(
        account.owner_uid,
        account.uid,
        &account.record_uuid,
        &account.home,
    )? {
        ExistingAccount::Ready => Ok(false),
        ExistingAccount::RetireUnusedReservation => {
            ensure!(
                receipt.owner == account.owner_uid,
                "unused creation reservation belongs to another normal user; it was retained"
            );
            if !retire_unused {
                return Ok(true);
            }
            ensure!(
                rustix::process::geteuid().is_root(),
                "administrator authentication is required to retire unused creation metadata"
            );
            let _creation = mutation_lock("creation", 0)?;
            let key = format!("{}.plist", account.name);
            ensure!(
                stored_file(&root, &key)?.as_ref() == Some(&bytes),
                "creation progress changed; metadata retained"
            );
            rustix::fs::unlinkat(&root, &key, rustix::fs::AtFlags::empty())?;
            root.sync_all()?;
            Ok(false)
        }
    }
}

pub(super) fn last_created(owner: u32) -> anyhow::Result<Option<Receipt>> {
    let path = Path::new(private_session::ROOT).join("account-creation");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let root = directory(&path, 0, false)?;
    let mut latest = None;
    for entry in rustix::fs::Dir::read_from(&root)? {
        let entry = entry?;
        let Some(name) = entry
            .file_name()
            .to_str()
            .ok()
            .and_then(|value| value.strip_suffix(".plist"))
        else {
            continue;
        };
        validate_name(name)?;
        let key = format!("{name}.plist");
        let Some(bytes) = stored_file(&root, &key)? else {
            continue;
        };
        let receipt: Receipt = plist::from_bytes(&bytes)?;
        receipt.validate(receipt.owner, name)?;
        if receipt.owner != owner {
            continue;
        }
        let file = File::from(openat(
            &root,
            &key,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let modified = file.metadata()?.modified()?;
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
            latest = Some((modified, receipt));
        }
    }
    Ok(latest.map(|(_, receipt)| receipt))
}

pub(super) fn completed_identity(
    owner: u32,
    name: &str,
) -> anyhow::Result<Option<crate::macos_setup::pending::Identity>> {
    validate_name(name)?;
    let path = Path::new(private_session::ROOT).join("account-creation");
    if !path.try_exists()? {
        return Ok(None);
    }
    let root = directory(&path, 0, false)?;
    let Some(bytes) = stored_file(&root, &format!("{name}.plist"))? else {
        return Ok(None);
    };
    let receipt: Receipt = plist::from_bytes(&bytes)?;
    receipt.validate(owner, name)?;
    Ok(
        (receipt.created && receipt.password_set).then_some(
            crate::macos_setup::pending::Identity {
                uid: receipt.uid,
                uuid: receipt.uuid,
            },
        ),
    )
}

pub(super) fn prepare(name: &str, endpoint: &Path) -> anyhow::Result<()> {
    let owner = root_owner()?;
    validate_name(name)?;
    let mut stream = connect(endpoint, owner)?;
    send(&mut stream, &serde_json::to_vec(&(owner, name))?)?;
    let password = Password::from_bytes(receive(&mut stream, 1024)?)?;
    let result = (|| -> anyhow::Result<()> {
        let _creation = mutation_lock("creation", 0)?;
        if owner_root()?.join(ACCOUNT).try_exists()? {
            ensure!(
                read_enrollment()?.name == name,
                "a different development account is enrolled; disable it before creating another"
            );
        }
        let root = directory(
            &Path::new(private_session::ROOT).join("account-creation"),
            0,
            true,
        )?;
        let _homes = directory(Path::new("/Users"), 0, false)?;
        let mut receipt = receipt(owner, name, &root)?;
        if !receipt.created {
            ensure!(
                !crate::account_uid::in_use(receipt.uid)?,
                "the reserved UID has live processes or a login domain; creation retained without changing its identity"
            );
        }
        glass_macos::account_setup::create_record(
            name,
            &receipt.uuid,
            receipt.uid,
            !receipt.created,
        )
        .map_err(anyhow::Error::msg)?;
        receipt.created = true;
        write_receipt(&root, &format!("{name}.plist"), &receipt)?;
        glass_macos::account_setup::set_password(
            name,
            &receipt.uuid,
            receipt.uid,
            password.bytes(),
            !receipt.password_set,
        )
        .map_err(anyhow::Error::msg)?;
        drop(password);
        receipt.password_set = true;
        write_receipt(&root, &format!("{name}.plist"), &receipt)?;
        enroll_with_options(name, true, false)
    })();
    let reply = result
        .as_ref()
        .map(|_| ())
        .map_err(|error| format!("{error:#}"));
    send(&mut stream, &serde_json::to_vec(&reply)?)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_name_and_uid_queries_validate_the_current_local_account_without_mutation() {
        let uid = rustix::process::getuid().as_raw();
        if uid < 501 {
            return;
        }
        let name =
            String::from_utf8(checked(Command::new("/usr/bin/id").arg("-un")).unwrap()).unwrap();
        let name = name.trim();
        let bytes = checked(Command::new("/usr/bin/dscl").args([
            "-plist",
            "/Local/Default",
            "-read",
            &format!("/Users/{name}"),
            "GeneratedUID",
        ]))
        .unwrap();
        let values: plist::Dictionary = plist::from_bytes(&bytes).unwrap();
        let uuid = values["dsAttrTypeStandard:GeneratedUID"]
            .as_array()
            .unwrap()[0]
            .as_string()
            .unwrap();
        assert!(glass_macos::account_setup::verify_identity(name, uuid, uid).is_ok());
        assert!(
            glass_macos::account_setup::verify_identity(
                name,
                "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                uid
            )
            .is_err()
        );
        assert!(glass_macos::account_setup::require_unused_name(name).is_err());
        let absent = format!(
            "glass_query_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        assert!(glass_macos::account_setup::require_unused_name(&absent).is_ok());
    }

    #[test]
    fn password_endpoint_is_private_and_drop_removes_only_its_own_socket() {
        let endpoint = Endpoint::new(rustix::process::getuid().as_raw()).unwrap();
        let path = endpoint.path.clone();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::metadata(endpoint.socket()).unwrap().mode() & 0o777,
            0o600
        );
        drop(endpoint);
        assert!(!path.exists());
    }

    #[test]
    fn oversized_password_header_is_refused_without_consuming_its_body() {
        let (mut input, mut output) = UnixStream::pair().unwrap();
        output.write_all(&1025u32.to_be_bytes()).unwrap();
        assert!(receive(&mut input, 1024).is_err());
    }

    #[test]
    fn ordinary_user_socket_cannot_impersonate_the_privileged_role() {
        if !rustix::process::geteuid().is_root() {
            let (input, _output) = UnixStream::pair().unwrap();
            assert!(
                private_session::authenticate_setup_root(
                    &input,
                    &[],
                    rustix::process::getuid().as_raw()
                )
                .is_err()
            );
        }
    }
}
