//! Start an enrolled desktop with transient local credentials and temporary service access.

use super::*;
use crate::macos_account::Password;
use crate::macos_setup::Progress;
use glass_macos::desktop_transport::{Credentials, Wire};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    account: Enrollment,
    group_uuid: Option<String>,
    originally_member: bool,
    grant_intent: bool,
    grant_confirmed: bool,
    authentication_dispatched: bool,
}

impl Record {
    fn validate(&self, account: &Enrollment) -> anyhow::Result<()> {
        ensure!(
            self.version == 1 && &self.account == account,
            "desktop startup belongs to a changed account; access state retained"
        );
        ensure!(
            !self.grant_intent || (!self.originally_member && self.group_uuid.is_some()),
            "invalid temporary desktop access record"
        );
        ensure!(
            !self.grant_confirmed || self.grant_intent,
            "invalid confirmed desktop access record"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "stage", deny_unknown_fields)]
enum Message {
    Hello {
        account: Enrollment,
        cleanup_only: bool,
    },
    Password,
    Granted,
    Finished {
        result: Result<(), String>,
    },
}

fn key(owner: u32) -> String {
    format!(".desktop-start-{owner}.json")
}

pub(super) fn pending(owner: u32) -> anyhow::Result<bool> {
    let path = Path::new(private_session::ROOT)
        .join("accounts")
        .join(key(owner));
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn remember(parent: &File, name: &str, record: &Record) -> anyhow::Result<()> {
    let temporary = format!(".desktop-{:032x}", rand::random::<u128>());
    let mut file = File::from(openat(
        parent,
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let result = (|| {
        private_session::require_write_protected(&file, 0)?;
        file.write_all(&serde_json::to_vec(record)?)?;
        rustix::fs::fchmod(&file, Mode::from_raw_mode(0o644))?;
        file.sync_all()?;
        rustix::fs::renameat(parent, &temporary, parent, name)?;
        parent.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(parent, &temporary, rustix::fs::AtFlags::empty());
    }
    result
}

fn restore_record(
    record: &Record,
    account: &Enrollment,
    mut inspect: impl FnMut() -> anyhow::Result<glass_macos::account_setup::DesktopAccess>,
    mut remove: impl FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    record.validate(account)?;
    if record.grant_intent {
        let state = inspect()?;
        ensure!(
            state.group_uuid == record.group_uuid,
            "Screen Sharing access group changed; temporary access requires attention"
        );
        if state.member {
            ensure!(
                record.grant_confirmed,
                "interrupted Screen Sharing authorization is ambiguous; access was retained. In System Settings → General → Sharing → Screen Sharing, review the development account's allowed-user entry and remove it if appropriate, then reopen Glass setup. Glass will not remove an unconfirmed grant"
            );
            remove(
                record
                    .group_uuid
                    .as_deref()
                    .context("temporary group identity missing")?,
            )?;
        }
        let state = inspect()?;
        ensure!(
            !state.member && state.group_uuid == record.group_uuid,
            "temporary Screen Sharing access removal is unconfirmed"
        );
    }
    Ok(())
}

fn restore(parent: &File, account: &Enrollment) -> anyhow::Result<()> {
    let name = key(account.owner_uid);
    let Some(bytes) = stored_file(parent, &name)? else {
        return Ok(());
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    restore_record(
        &record,
        account,
        || {
            glass_macos::account_setup::desktop_access(
                &account.name,
                &account.record_uuid,
                account.uid,
            )
            .map_err(anyhow::Error::msg)
        },
        |group| {
            glass_macos::account_setup::set_desktop_access(
                &account.name,
                &account.record_uuid,
                account.uid,
                group,
                false,
            )
            .map(|_| ())
            .map_err(anyhow::Error::msg)
        },
    )?;
    rustix::fs::unlinkat(parent, &name, rustix::fs::AtFlags::empty())?;
    parent.sync_all()?;
    Ok(())
}

pub(super) fn restore_pending(account: &Enrollment) -> anyhow::Result<()> {
    if !pending(account.owner_uid)? {
        return Ok(());
    }
    let parent = directory(&Path::new(private_session::ROOT).join("accounts"), 0, false)?;
    restore(&parent, account)
}

pub(super) fn choice(name: &str) -> anyhow::Result<glass_macos::setup_window::EnvironmentStart> {
    let account = read_enrollment()?;
    ensure!(account.name == name, "enrolled account changed");
    Ok(glass_macos::setup_window::EnvironmentStart {
        name: name.into(),
        needs_password: needs_password(pending(account.owner_uid)?, || {
            private_session::live_login_observation(account.uid)
        })?,
    })
}

fn needs_password(
    cleanup_only: bool,
    login: impl FnOnce() -> std::io::Result<private_session::LoginObservation>,
) -> anyhow::Result<bool> {
    if cleanup_only {
        return Ok(false);
    }
    match login()? {
        private_session::LoginObservation::NotObserved => Ok(true),
        private_session::LoginObservation::Complete => Ok(false),
        private_session::LoginObservation::Incomplete => bail!(
            "macOS is still initializing the development login; no new authentication was attempted. Wait for it to finish, then reopen application setup to check its status"
        ),
    }
}

fn server_peer(port: u16) -> anyhow::Result<private_session::ProcessIdentity> {
    let tuple = format!("n127.0.0.1:5900->127.0.0.1:{port}");
    let lookup = || -> anyhow::Result<u32> {
        let output = glass_core::run_bounded(
            Command::new("/usr/sbin/lsof").args(["-nP", "-iTCP:5900", "-sTCP:ESTABLISHED", "-Fpn"]),
            Duration::from_secs(5),
            "checking the local Apple desktop service",
        )?;
        ensure!(
            output.status.success()
                && output.stderr.is_empty()
                && output.stdout.len() <= 1024 * 1024,
            "local desktop service identity unavailable"
        );
        server_pid(&output.stdout, &tuple)
    };
    let pid = lookup()?;
    let before = private_session::apple_server_identity(pid)?;
    ensure!(
        lookup()? == pid && private_session::apple_server_identity(pid)? == before,
        "local desktop service changed"
    );
    Ok(before)
}

fn server_pid(bytes: &[u8], tuple: &str) -> anyhow::Result<u32> {
    let mut pid = None;
    let mut found = Vec::new();
    for line in std::str::from_utf8(bytes)?.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = Some(value.parse::<u32>()?);
        }
        if line == tuple {
            found.push(pid.context("desktop service has no process")?);
        }
    }
    ensure!(
        found.len() == 1 && found[0] > 1,
        "local desktop service connection is ambiguous"
    );
    Ok(found[0])
}

fn prepare_wire() -> anyhow::Result<glass_macos::desktop_transport::Prepared> {
    let stream = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, 5900)), Duration::from_secs(3))
        .context("Enable Screen Sharing in System Settings → General → Sharing before starting the development desktop")?;
    let prepared = Wire::new(stream, Duration::from_secs(30))?.prepare()?;
    server_peer(prepared.local_port()?)?;
    Ok(prepared)
}

fn send_message(stream: &mut UnixStream, message: &Message) -> anyhow::Result<()> {
    account_creation::send(stream, &serde_json::to_vec(message)?)
}

fn worker_finished(stream: &mut UnixStream) -> anyhow::Result<()> {
    let response = account_creation::receive(stream, 1)?;
    ensure!(
        &*response == b"R" || &*response == b"F",
        "owner did not return a recognized worker-verification outcome"
    );
    Ok(())
}

fn wait_private_worker(
    cancelled: &AtomicBool,
    session: u32,
    deadline: Instant,
    mut current_session: impl FnMut() -> std::io::Result<u32>,
    mut probe: impl FnMut() -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    loop {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "desktop worker check cancelled"
        );
        ensure!(
            current_session()? == session,
            "normal desktop changed during worker verification"
        );
        let problem = match probe() {
            Ok(true) => return Ok(()),
            Ok(false) => "the authenticated worker is not attached to its private GUI".to_owned(),
            Err(error) => format!("{error:#}").chars().take(2048).collect(),
        };
        ensure!(
            Instant::now() < deadline,
            "desktop was started, but worker verification did not finish: {problem}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn finish_owner(primary: Option<anyhow::Error>, cleanup: Result<(), String>) -> anyhow::Result<()> {
    match (primary, cleanup) {
        (None, result) => result.map_err(anyhow::Error::msg),
        (Some(error), Ok(())) => Err(error),
        (Some(error), Err(cleanup)) => {
            Err(anyhow::anyhow!("{error:#}; startup cleanup: {cleanup}"))
        }
    }
}

fn record_grant_result(
    record: &mut Record,
    result: Result<
        glass_macos::account_setup::DesktopAccess,
        glass_macos::account_setup::DesktopAccessError,
    >,
    mut persist: impl FnMut(&Record) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if result.is_ok()
        || result
            .as_ref()
            .err()
            .is_some_and(|error| error.mutation_acknowledged)
    {
        record.grant_confirmed = true;
        persist(record)?;
    }
    result.map(|_| ()).map_err(anyhow::Error::new)
}

fn grant_worker_access(
    stream: &mut UnixStream,
    prepare: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    prepare()?;
    send_message(stream, &Message::Granted)
}

/// Internal root role, reached only after native administrator approval and peer authentication.
pub(super) fn privileged(endpoint: &Path) -> anyhow::Result<()> {
    let owner = root_owner()?;
    let _owner = mutation_lock("owner", owner)?;
    let account = read_enrollment()?;
    ensure!(account.owner_uid == owner, "desktop startup owner changed");
    let _worker = mutation_lock("worker", account.uid)?;
    let registration = directory(&owner_root()?, 0, false)?;
    ensure!(
        stored_file(&registration, "disabled")?.is_none(),
        "desktop enrollment is disabled"
    );
    let mut stream = account_creation::connect(endpoint, owner)?;
    stream.set_read_timeout(None)?;
    let cleanup_only = pending(owner)?;
    send_message(
        &mut stream,
        &Message::Hello {
            account: account.clone(),
            cleanup_only,
        },
    )?;
    let parent = directory(&Path::new(private_session::ROOT).join("accounts"), 0, false)?;
    let result = (|| -> anyhow::Result<()> {
        restore(&parent, &account)?;
        if cleanup_only {
            return Ok(());
        }
        ensure!(
            account == account_record(&account.name, owner, true)?,
            "development account changed"
        );
        if needs_password(false, || {
            private_session::live_login_for(owner, account.uid)
        })? {
            // This connection is credential-free and is closed before any human wait.
            drop(prepare_wire()?);
            send_message(&mut stream, &Message::Password)?;
            let password = Password::from_bytes(account_creation::receive(&mut stream, 1024)?)?;
            let credentials = Credentials::new(account.name.as_bytes(), password.bytes())?;
            drop(password);
            let state = glass_macos::account_setup::desktop_access(
                &account.name,
                &account.record_uuid,
                account.uid,
            )
            .map_err(anyhow::Error::msg)?;
            let mut record = Record {
                version: 1,
                account: account.clone(),
                group_uuid: state.group_uuid,
                originally_member: state.member,
                grant_intent: false,
                grant_confirmed: false,
                authentication_dispatched: false,
            };
            remember(&parent, &key(owner), &record)?;
            if !state.authorized {
                ensure!(
                    !record.originally_member,
                    "existing Screen Sharing membership is not authorized; check its macOS access policy"
                );
                let group = record
                    .group_uuid
                    .as_deref()
                    .context("Screen Sharing access group is unavailable")?
                    .to_owned();
                record.grant_intent = true;
                remember(&parent, &key(owner), &record)?;
                let changed = glass_macos::account_setup::set_desktop_access(
                    &account.name,
                    &account.record_uuid,
                    account.uid,
                    &group,
                    true,
                );
                record_grant_result(&mut record, changed, |record| {
                    remember(&parent, &key(owner), record)
                })?;
            }
            let prepared = prepare_wire()?;
            record.authentication_dispatched = true;
            remember(&parent, &key(owner), &record)?;
            let granted = prepared
                .authenticate(credentials)?
                .private_offer()?
                .select_private()?;
            grant_worker_access(&mut stream, || {
                worker_activation::activate(&registration, &account)
            })?;
            stream.set_read_timeout(Some(Duration::from_secs(90)))?;
            worker_finished(&mut stream)?;
            drop(granted);
        } else {
            grant_worker_access(&mut stream, || {
                worker_activation::activate(&registration, &account)
            })?;
            stream.set_read_timeout(Some(Duration::from_secs(90)))?;
            worker_finished(&mut stream)?;
        }
        Ok(())
    })();
    let cleanup = restore(&parent, &account);
    let result = match (result, cleanup) {
        (result, Ok(())) => result,
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("temporary desktop access cleanup: {cleanup:#}")))
        }
    };
    send_message(
        &mut stream,
        &Message::Finished {
            result: result.map_err(|error| format!("{error:#}")),
        },
    )
}

#[derive(Clone, Copy)]
pub(super) enum Outcome {
    Ready,
    CleanupFinished,
}

pub(super) fn run(
    name: &str,
    cancelled: &AtomicBool,
    mut password: impl FnMut() -> anyhow::Result<Password>,
    mut progress: impl FnMut(Progress),
) -> anyhow::Result<Outcome> {
    let _setup = guided::lock_setup()?;
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        "desktop startup cancelled before authorization"
    );
    let session = private_session::owner_desktop_session()?;
    let account = read_enrollment()?;
    ensure!(
        private_session::is_owner() && account.name == name,
        "desktop startup requires the current enabled enrollment"
    );
    ensure!(
        name.len() <= 63,
        "local macOS desktop startup supports account names up to 63 UTF-8 bytes; the account was not changed"
    );
    let lease = File::open(owner_root()?.join(ACCOUNT))?;
    fs4::FileExt::try_lock(&lease)
        .context("stop Glass application work before starting its desktop")?;
    if !pending(account.owner_uid)?
        && let Ok(readiness) =
            crate::macos_worker::backend_transport::readiness_with_lease(lease.try_clone()?)
        && readiness.phase() == crate::macos_setup::Phase::Ready
    {
        return Ok(Outcome::Ready);
    }
    let requirement = private_session::signing_requirement()?;
    let endpoint = account_creation::Endpoint::new(account.owner_uid)?;
    let socket = endpoint.socket();
    let path = socket.to_str().context("desktop endpoint is not UTF-8")?;
    let mut cleanup_only = false;
    let started = std::thread::scope(|scope| -> anyhow::Result<()> {
        progress(Progress::applying(
            "Complete or cancel macOS administrator approval locally; there is no deadline.",
        ));
        let authorization =
            scope.spawn(|| elevate_using(&["macos-start-environment", path], false, true));
        let mut stream = loop {
            match endpoint.listener.accept() {
                Ok((stream, _))
                    if private_session::authenticate_setup_root(
                        &stream,
                        &requirement,
                        account.owner_uid,
                    )
                    .is_ok() =>
                {
                    break stream;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if authorization.is_finished() {
                        authorization
                            .join()
                            .map_err(|_| anyhow::anyhow!("desktop authorization stopped"))??;
                        bail!("desktop helper exited before connecting");
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let exchange = (|| -> anyhow::Result<()> {
            let mut supplied_password = false;
            let mut worker_failure = None;
            let hello: Message =
                serde_json::from_slice(&account_creation::receive(&mut stream, 8192)?)?;
            let Message::Hello {
                account: received,
                cleanup_only: cleanup,
            } = hello
            else {
                bail!("desktop helper introduction missing");
            };
            ensure!(received == account, "desktop helper account changed");
            cleanup_only = cleanup;
            loop {
                let received = account_creation::receive(&mut stream, 8192).and_then(|bytes| {
                    serde_json::from_slice::<Message>(&bytes).map_err(Into::into)
                });
                let message = match received {
                    Ok(message) => message,
                    Err(error) => {
                        return finish_owner(
                            worker_failure,
                            Err(format!("receiving the helper outcome: {error:#}")),
                        );
                    }
                };
                match message {
                    Message::Password => {
                        ensure!(
                            !cleanup_only,
                            "cleanup cannot request account authentication"
                        );
                        ensure!(
                            !supplied_password,
                            "desktop authentication is not automatically retried"
                        );
                        supplied_password = true;
                        ensure!(
                            !cancelled.load(Ordering::Relaxed)
                                && private_session::owner_desktop_session()? == session,
                            "desktop startup cancelled or normal desktop changed"
                        );
                        progress(Progress::applying(
                            "Authenticating the development account locally. Glass does not save its password or retry authentication automatically.",
                        ));
                        let value = password()?;
                        ensure!(
                            value.bytes().len() <= 63,
                            "local macOS desktop startup supports passwords up to 63 UTF-8 bytes; the password was not changed"
                        );
                        account_creation::send(&mut stream, value.bytes())?;
                    }
                    Message::Granted => {
                        ensure!(!cleanup_only, "cleanup cannot dispatch desktop startup");
                        progress(Progress::waiting(
                            "The development desktop is available. Checking its installed Glass worker…",
                        ));
                        let checked = wait_private_worker(
                            cancelled,
                            session,
                            Instant::now() + Duration::from_secs(60),
                            private_session::owner_desktop_session,
                            || {
                                let readiness =
                                    crate::macos_worker::backend_transport::readiness_with_lease(
                                        lease.try_clone()?,
                                    )
                                    .map_err(anyhow::Error::msg)?;
                                Ok(readiness.private_session)
                            },
                        );
                        let response: &[u8] = if checked.is_ok() { b"R" } else { b"F" };
                        worker_failure = checked.err();
                        if let Err(error) = account_creation::send(&mut stream, response) {
                            return finish_owner(
                                worker_failure,
                                Err(format!("notifying the helper: {error:#}")),
                            );
                        }
                        progress(Progress::applying(
                            "Finishing temporary desktop-access cleanup. Complete or cancel any macOS approval locally.",
                        ));
                    }
                    Message::Finished { result } => return finish_owner(worker_failure, result),
                    Message::Hello { .. } => bail!("duplicate desktop helper introduction"),
                }
            }
        })();
        drop(stream);
        let authorized = authorization
            .join()
            .map_err(|_| anyhow::anyhow!("desktop authorization stopped"))?;
        match (exchange, authorized) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(error), Err(auth)) => {
                Err(anyhow::anyhow!("{error:#}; privileged helper: {auth:#}"))
            }
        }
    });
    started?;
    ensure!(
        private_session::owner_desktop_session()? == session,
        "normal desktop changed while startup finished"
    );
    if cleanup_only {
        return Ok(Outcome::CleanupFinished);
    }
    use sha2::Digest;
    let binding = sha2::Sha256::digest(std::fs::read(owner_root()?.join(ACCOUNT))?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    progress(Progress::applying(
        "Desktop started. Verifying any interrupted workspace recovery…",
    ));
    crate::macos_worker::backend_transport::finish_permission_recovery(account.uid, binding, &lease)
        .map(|()| Outcome::Ready)
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_macos::account_setup::DesktopAccess;
    use std::cell::Cell;

    fn record() -> Record {
        Record {
            version: 1,
            account: Enrollment {
                version: 1,
                uid: 502,
                gid: 20,
                record_uuid: "user-uuid".into(),
                name: "development".into(),
                home: "/Users/development".into(),
                owner_uid: 501,
            },
            group_uuid: Some("group-uuid".into()),
            originally_member: false,
            grant_intent: true,
            grant_confirmed: true,
            authentication_dispatched: true,
        }
    }
    fn state(member: bool) -> DesktopAccess {
        DesktopAccess {
            authorized: member,
            member,
            group_uuid: Some("group-uuid".into()),
        }
    }

    #[test]
    fn worker_timeout_preserves_its_last_connection_error() {
        let cancelled = AtomicBool::new(false);
        let error = wait_private_worker(
            &cancelled,
            7,
            Instant::now(),
            || Ok(7),
            || bail!("Connection refused (os error 61)"),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Connection refused (os error 61)")
        );
    }

    #[test]
    fn worker_cancellation_never_dispatches_a_probe() {
        assert!(
            wait_private_worker(
                &AtomicBool::new(true),
                7,
                Instant::now(),
                || Ok(7),
                || panic!("cancelled probe")
            )
            .is_err()
        );
    }

    #[test]
    fn worker_failure_is_primary_when_cleanup_transport_also_fails() {
        let error = finish_owner(
            Some(anyhow::anyhow!("Connection refused (os error 61)")),
            Err("Broken pipe (os error 32)".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("Connection refused"));
        assert!(error.contains("Broken pipe"));
    }

    #[test]
    fn failed_worker_verification_is_a_cleanup_handshake_not_a_disconnect() {
        for response in [b"R", b"F"] {
            let (mut owner, mut helper) = UnixStream::pair().unwrap();
            account_creation::send(&mut owner, response).unwrap();
            worker_finished(&mut helper).unwrap();
        }
        let (mut owner, mut helper) = UnixStream::pair().unwrap();
        account_creation::send(&mut owner, b"X").unwrap();
        assert!(worker_finished(&mut helper).is_err());
    }

    #[test]
    fn acknowledged_directory_write_is_durable_even_when_verification_fails() {
        let mut record = record();
        record.grant_confirmed = false;
        let failure = glass_macos::account_setup::DesktopAccessError {
            status: 5,
            stage: 6,
            native_domain: 1,
            native_code: 123,
            mutation_acknowledged: true,
            direct_member: None,
        };
        let mut persisted = false;
        let result = record_grant_result(&mut record, Err(failure), |record| {
            assert!(record.grant_confirmed);
            persisted = true;
            Ok(())
        });
        assert!(persisted);
        assert!(result.is_err());
        let present = std::cell::Cell::new(true);
        restore_record(
            &record,
            &record.account,
            || Ok(state(present.get())),
            |_| {
                present.set(false);
                Ok(())
            },
        )
        .unwrap();
        assert!(!present.get());
    }

    #[test]
    fn unacknowledged_directory_failure_cannot_claim_or_remove_a_grant() {
        let mut record = record();
        record.grant_confirmed = false;
        let failure = glass_macos::account_setup::DesktopAccessError {
            status: 5,
            stage: 5,
            native_domain: 1,
            native_code: 123,
            mutation_acknowledged: false,
            direct_member: None,
        };
        assert!(
            record_grant_result(&mut record, Err(failure), |_| {
                panic!("unacknowledged write cannot create cleanup authority")
            })
            .is_err()
        );
        assert!(!record.grant_confirmed);
    }

    #[test]
    fn invalid_activation_socket_never_advertises_worker_access() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("worker.sock"), b"preserved unrelated file").unwrap();
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let account = record().account;
        assert!(
            grant_worker_access(&mut sender, || wait_for_socket(
                &File::open(root.path()).unwrap(),
                account.owner_uid,
                account.uid
            ))
            .is_err()
        );
        let mut byte = [0; 1];
        assert_eq!(
            receiver.read(&mut byte).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            std::fs::read(root.path().join("worker.sock")).unwrap(),
            b"preserved unrelated file"
        );
    }

    #[test]
    fn worker_access_is_validated_before_granted_even_for_a_retained_desktop() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("worker.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut account = record().account;
        account.owner_uid = rustix::process::getuid().as_raw();
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        grant_worker_access(&mut sender, || {
            wait_for_socket(
                &File::open(root.path()).unwrap(),
                account.owner_uid,
                account.uid,
            )
        })
        .unwrap();
        let message: Message =
            serde_json::from_slice(&account_creation::receive(&mut receiver, 8192).unwrap())
                .unwrap();
        assert!(matches!(message, Message::Granted));
        let info = std::fs::symlink_metadata(socket).unwrap();
        assert_eq!(info.uid(), account.owner_uid);
        assert_eq!(info.mode() & 0o777, 0o600);
    }

    #[test]
    fn incomplete_login_neither_reuses_a_desktop_nor_prompts_for_new_credentials() {
        use private_session::LoginObservation::{Complete, Incomplete, NotObserved};
        assert!(needs_password(false, || Ok(NotObserved)).unwrap());
        assert!(!needs_password(false, || Ok(Complete)).unwrap());
        assert!(needs_password(false, || Ok(Incomplete)).is_err());
        assert!(
            !needs_password(true, || panic!(
                "pending cleanup must not depend on login state"
            ))
            .unwrap()
        );
    }

    #[test]
    fn recovery_covers_both_sides_of_the_grant_and_authentication_dispatch() {
        for granted in [false, true] {
            for dispatched in [false, true] {
                let mut record = record();
                record.authentication_dispatched = dispatched;
                let present = Cell::new(granted);
                let removals = Cell::new(0);
                restore_record(
                    &record,
                    &record.account,
                    || Ok(state(present.get())),
                    |group| {
                        assert_eq!(group, "group-uuid");
                        present.set(false);
                        removals.set(removals.get() + 1);
                        Ok(())
                    },
                )
                .unwrap();
                assert!(!present.get());
                assert_eq!(removals.get(), usize::from(granted));
                // Repeating cleanup never authenticates or adds access.
                restore_record(
                    &record,
                    &record.account,
                    || Ok(state(present.get())),
                    |_| panic!("already absent"),
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn interrupted_add_never_removes_an_unconfirmed_or_independent_grant() {
        let mut record = record();
        record.grant_confirmed = false;
        assert!(
            restore_record(
                &record,
                &record.account,
                || Ok(state(true)),
                |_| panic!("unconfirmed ownership")
            )
            .is_err()
        );
        restore_record(
            &record,
            &record.account,
            || Ok(state(false)),
            |_| panic!("already absent"),
        )
        .unwrap();
    }

    #[test]
    fn preexisting_and_unmodified_access_never_calls_directory_mutation() {
        for member in [false, true] {
            let mut record = record();
            record.originally_member = member;
            record.grant_intent = false;
            record.grant_confirmed = false;
            restore_record(
                &record,
                &record.account,
                || panic!("no temporary grant"),
                |_| panic!("must preserve existing access"),
            )
            .unwrap();
        }
    }

    #[test]
    fn changed_account_or_group_retains_evidence_without_removing_access() {
        let record = record();
        let mut changed = record.account.clone();
        changed.record_uuid = "replacement".into();
        assert!(
            restore_record(
                &record,
                &changed,
                || panic!("account binding first"),
                |_| panic!("unbound removal")
            )
            .is_err()
        );
        let mut changed = state(true);
        changed.group_uuid = Some("replacement".into());
        assert!(
            restore_record(
                &record,
                &record.account,
                || Ok(changed.clone()),
                |_| panic!("unbound group removal")
            )
            .is_err()
        );
    }

    #[test]
    fn unconfirmed_or_failed_removal_is_not_success() {
        let record = record();
        assert!(
            restore_record(
                &record,
                &record.account,
                || Ok(state(true)),
                |_| bail!("injected directory error")
            )
            .is_err()
        );
        assert!(restore_record(&record, &record.account, || Ok(state(true)), |_| Ok(())).is_err());
    }

    #[test]
    fn malformed_or_future_records_do_not_authorize_cleanup() {
        let mut record = record();
        record.version = 2;
        assert!(record.validate(&record.account).is_err());
        record.version = 1;
        record.originally_member = true;
        assert!(record.validate(&record.account).is_err());
        record.originally_member = false;
        record.group_uuid = None;
        assert!(record.validate(&record.account).is_err());
        assert!(serde_json::from_str::<Record>(r#"{"version":1,"password":"forbidden"}"#).is_err());
    }

    #[test]
    fn service_identity_selects_only_the_exact_reverse_connection() {
        let tuple = "n127.0.0.1:5900->127.0.0.1:41234";
        let listing = b"p10\nf12\nn127.0.0.1:5900->127.0.0.1:41235\np20\nf17\nn127.0.0.1:5900->127.0.0.1:41234\n";
        assert_eq!(server_pid(listing, tuple).unwrap(), 20);
        for invalid in [
            b"".as_slice(),
            b"n127.0.0.1:5900->127.0.0.1:41234\n",
            b"p20\nn127.0.0.1:5900->127.0.0.1:41234\np21\nn127.0.0.1:5900->127.0.0.1:41234\n",
        ] {
            assert!(server_pid(invalid, tuple).is_err());
        }
    }
}
