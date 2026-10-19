//! a connection and workspace lease owned by one macOS backend instance.

use std::fs::File;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use glass_core::{Deadline, GlassError, Result};

use super::*;
use crate::backend_worker::{Operation, Request, Response};
use crate::remote_backend::Connection;
use crate::workspace_permissions::{NativePrepare, PreparationError};

const MAX_MESSAGE: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CleanupPhase {
    #[default]
    WaitingForIdle,
    AccessRestored,
    JournalFinished,
    Complete,
}

impl CleanupPhase {
    fn advance(
        &mut self,
        wait: impl FnOnce() -> Result<()>,
        restore: impl FnOnce() -> Result<()>,
        finish: impl FnOnce() -> Result<()>,
        unlock: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if *self == Self::WaitingForIdle {
            wait()?;
            restore()?;
            *self = Self::AccessRestored;
        }
        if *self == Self::AccessRestored {
            finish()?;
            *self = Self::JournalFinished;
        }
        if *self == Self::JournalFinished {
            unlock()?;
            *self = Self::Complete;
        }
        Ok(())
    }
}

fn exit_and_cleanup(receipt: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (receipt, cleanup) {
        (Ok(()), result) => result,
        (Err(error), Ok(())) => Err(error),
        (Err(primary), Err(cleanup)) => Err(GlassError::cleanup_failed(
            "restoring backend after worker exit failure",
            primary,
            cleanup,
        )),
    }
}

pub(crate) struct NativeConnection {
    _environment_lease: crate::macos_setup::Lease,
    stream: UnixStream,
    lease: File,
    prepare: Option<NativePrepare>,
    established: bool,
    base: PathBuf,
    buffer: Vec<u8>,
    sequence: u64,
    clean_exit: bool,
    exit_received: bool,
    cleanup: CleanupPhase,
    receipt_failure: Option<String>,
    poisoned: bool,
    journal: Arc<Mutex<crate::permission_journal::PermissionJournal>>,
}

fn error(error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(error.to_string())
}

fn bounded(deadline: Deadline, own: Option<Duration>) -> Deadline {
    own.map_or(deadline, |duration| {
        Deadline::at(deadline.resolve(std::time::Instant::now() + duration).0)
    })
}

fn ready(
    stream: &UnixStream,
    deadline: Deadline,
    interest: rustix::event::PollFlags,
) -> Result<()> {
    loop {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("backend worker transport"));
        }
        let remaining = deadline
            .remaining()
            .map(rustix::event::Timespec::try_from)
            .transpose()
            .map_err(error)?;
        let mut poll = [rustix::event::PollFd::new(stream, interest)];
        match rustix::event::poll(&mut poll, remaining.as_ref()) {
            Ok(0) => continue,
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(failure) => return Err(error(failure)),
        }
    }
}

fn write(stream: &mut UnixStream, mut bytes: &[u8], deadline: Deadline) -> Result<()> {
    while !bytes.is_empty() {
        ready(stream, deadline, rustix::event::PollFlags::OUT)?;
        match stream.write(bytes) {
            Ok(0) => return Err(error("backend worker write returned zero")),
            Ok(count) => bytes = &bytes[count..],
            Err(failure)
                if matches!(
                    failure.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(failure) => return Err(error(failure)),
        }
    }
    Ok(())
}

fn read(stream: &mut UnixStream, mut bytes: &mut [u8], deadline: Deadline) -> Result<()> {
    while !bytes.is_empty() {
        ready(stream, deadline, rustix::event::PollFlags::IN)?;
        match stream.read(bytes) {
            Ok(0) => return Err(error("backend worker connection ended before its frame")),
            Ok(count) => bytes = &mut bytes[count..],
            Err(failure)
                if matches!(
                    failure.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(failure) => return Err(error(failure)),
        }
    }
    Ok(())
}

fn send(
    stream: &mut UnixStream,
    kind: u8,
    data: &[u8],
    deadline: Deadline,
    own: Option<Duration>,
) -> Result<()> {
    if data.len() > MAX_FRAME {
        return Err(error("backend worker frame exceeds limit").before_dispatch());
    }
    let deadline = bounded(deadline, own);
    write(stream, &[kind], deadline)?;
    write(stream, &(data.len() as u32).to_be_bytes(), deadline)?;
    write(stream, data, deadline)
}

fn receive(
    stream: &mut UnixStream,
    deadline: Deadline,
    own: Option<Duration>,
) -> Result<(u8, Vec<u8>)> {
    let mut header = [0; 5];
    let deadline = bounded(deadline, own);
    read(stream, &mut header, deadline)?;
    let size = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if size > MAX_FRAME {
        return Err(error("backend worker frame exceeds limit"));
    }
    let mut bytes = vec![0; size];
    read(stream, &mut bytes, deadline)?;
    Ok((header[0], bytes))
}

struct BrokerConnection {
    stream: UnixStream,
    lease: File,
    greeting: Greeting,
}

fn connect_broker(lease: Option<File>) -> Result<BrokerConnection> {
    let owner = rustix::process::geteuid().as_raw();
    native::protected_endpoint(owner).map_err(error)?;
    let lease = match lease {
        Some(lease) => lease,
        None => {
            let lease =
                File::open(glass_macos::private_session::root_for(owner).join("account.plist"))
                    .map_err(error)?;
            fs4::FileExt::try_lock(&lease)
                .map_err(|_| error("the private macOS environment is already in use"))?;
            lease
        }
    };
    let callback = super::callback::Callback::new(owner)?;
    let requirement = super::callback::broker_requirement()?;
    let mut activation =
        UnixStream::connect(glass_macos::private_session::root_for(owner).join("worker.sock"))
            .map_err(error)?;
    activation.set_nonblocking(true).map_err(error)?;
    if glass_macos::worker_socket::peer_uid(&activation).map_err(error)? != 0 {
        return Err(error("activation socket was not created by launchd"));
    }
    send(
        &mut activation,
        ACTIVATE,
        &serde_json::to_vec(&Activation {
            version: PROTOCOL,
            endpoint: callback.path(),
        })
        .map_err(error)?,
        Deadline::from_millis(10_000),
        None,
    )?;
    let worker = glass_macos::private_session::worker_uid();
    let (mut stream, authenticated_pid, authenticated_session) =
        callback.accept(worker, &requirement, Deadline::UNBOUNDED)?;
    drop(activation);
    drop(callback);
    let (kind, bytes) = receive(&mut stream, Deadline::from_millis(10_000), None)?;
    let greeting: Greeting = serde_json::from_slice(&bytes).map_err(error)?;
    if kind != GREETING
        || greeting.version != PROTOCOL
        || greeting.worker != worker
        || greeting.session != authenticated_session
        || !greeting.process.matches_pid(authenticated_pid)
        || !glass_macos::private_session::account_processes(worker)
            .map_err(error)?
            .contains(&greeting.process)
    {
        return Err(error(
            "worker did not bind a live private session before workspace preparation",
        ));
    }
    Ok(BrokerConnection {
        stream,
        lease,
        greeting,
    })
}

pub(crate) fn readiness() -> Result<crate::macos_setup::Readiness> {
    probe(None, PROBE)
}

pub(crate) fn readiness_with_lease(lease: File) -> Result<crate::macos_setup::Readiness> {
    probe(Some(lease), PROBE)
}

pub(crate) fn open_onboarding() -> Result<crate::macos_setup::Readiness> {
    probe(None, OPEN_ONBOARDING)
}

fn probe(lease: Option<File>, request: u8) -> Result<crate::macos_setup::Readiness> {
    let mut connection = connect_broker(lease)?;
    let deadline = Deadline::from_millis(10_000);
    send(&mut connection.stream, request, &[], deadline, None)?;
    let (kind, bytes) = receive(&mut connection.stream, deadline, None)?;
    if kind != READINESS {
        if kind == FAILED {
            let reason: String = serde_json::from_slice(&bytes).map_err(error)?;
            return Err(error(reason));
        }
        return Err(error("private worker did not return readiness facts"));
    }
    let readiness = serde_json::from_slice(&bytes).map_err(error)?;
    drop(connection.stream);
    let worker = glass_macos::private_session::worker_uid();
    let deadline = Deadline::from_millis(8000);
    while glass_macos::private_session::account_processes(worker)
        .map_err(error)?
        .contains(&connection.greeting.process)
    {
        if deadline.has_passed() {
            return Err(error(
                "readiness worker has not exited; setup state is unconfirmed",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(readiness)
}

impl NativeConnection {
    pub(crate) fn connect(environment_lease: crate::macos_setup::Lease) -> Result<Self> {
        let owner = rustix::process::geteuid().as_raw();
        let BrokerConnection {
            stream,
            lease,
            greeting,
        } = connect_broker(None)?;
        if !greeting.private_session {
            return Err(error(
                "switch back to your normal account before starting private application work",
            ));
        }
        use sha2::Digest;
        let account = sha2::Sha256::digest(
            std::fs::read(glass_macos::private_session::root_for(owner).join("account.plist"))
                .map_err(error)?,
        )
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
        let journal = Arc::new(Mutex::new(
            crate::permission_journal::PermissionJournal::open(
                &crate::permission_journal::PermissionJournal::root().map_err(error)?,
                glass_macos::private_session::worker_uid(),
                account,
                Some(greeting.session),
                Some(greeting.process),
            )
            .map_err(error)?,
        ));
        let mut connection = Self {
            _environment_lease: environment_lease,
            stream,
            lease,
            prepare: None,
            established: false,
            base: PathBuf::new(),
            buffer: Vec::new(),
            sequence: 0,
            clean_exit: false,
            exit_received: false,
            cleanup: CleanupPhase::default(),
            receipt_failure: None,
            poisoned: false,
            journal,
        };
        let initialized = connection.initialize();
        connection.finish_connect(initialized)
    }

    fn finish_connect(mut self, initialized: Result<()>) -> Result<Self> {
        if let Err(primary) = initialized {
            return match self.abort_connect(Deadline::from_millis(8000)) {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(GlassError::cleanup_failed(
                    "restoring failed private backend bootstrap",
                    primary,
                    cleanup,
                )),
            };
        }
        self.established = true;
        Ok(self)
    }

    fn initialize(&mut self) -> Result<()> {
        let journal_root = self
            .journal
            .lock()
            .map_err(|_| error("permission journal is poisoned"))?
            .protected_root()
            .to_path_buf();
        let bootstrap = Bootstrap::new().map_err(error)?;
        let home = glass_macos::private_session::owner_home()
            .ok_or_else(|| error("enrolled owner home unavailable"))?
            .canonicalize()
            .map_err(error)?;
        self.prepare = Some(
            NativePrepare::for_backend(
                glass_macos::private_session::worker_uid(),
                bootstrap.cwd.clone(),
                home,
                bootstrap.env.get("PATH").cloned(),
                vec![
                    PathBuf::from(glass_macos::private_session::ROOT),
                    journal_root,
                ],
                self.journal.clone(),
            )
            .map_err(error)?,
        );
        self.base = bootstrap.cwd.clone();
        let deadline = Deadline::from_millis(10_000);
        send(
            &mut self.stream,
            HELLO,
            &serde_json::to_vec(&bootstrap).map_err(error)?,
            deadline,
            None,
        )?;
        let (kind, data) = receive(&mut self.stream, deadline, None)?;
        if kind == FAILED {
            return Err(error(
                serde_json::from_slice::<String>(&data).map_err(error)?,
            ));
        }
        let admitted: Admission = serde_json::from_slice(&data).map_err(error)?;
        if kind != ACCEPTED || admitted.version != PROTOCOL || !admitted.private_attachment_checked
        {
            return Err(error("private worker did not attest its attachment"));
        }
        Ok(())
    }

    fn abort_connect(&mut self, deadline: Deadline) -> Result<()> {
        // EOF cancels both pre-HELLO setup and an in-flight native-backend spawn.
        let _ = self.stream.shutdown(Shutdown::Both);
        self.restore_permissions(deadline)
    }

    fn receipt(&mut self, bytes: &[u8]) -> Result<()> {
        self.exit_received = true;
        let result = (|| {
            let receipt: Receipt = serde_json::from_slice(bytes).map_err(error)?;
            self.clean_exit = receipt.clean_worker_exit
                && receipt.connection_valid
                && !receipt.forced_termination
                && receipt.worker_exit_code == Some(0);
            if self.clean_exit {
                Ok(())
            } else {
                Err(error(
                    "private worker exited abnormally or without valid cleanup confirmation",
                ))
            }
        })();
        self.receipt_failure = result.as_ref().err().map(ToString::to_string);
        result
    }

    fn restore_permissions(&mut self, deadline: Deadline) -> Result<()> {
        let journal = &self.journal;
        let prepare = &mut self.prepare;
        let lease = &self.lease;
        self.cleanup.advance(
            || {
                let journal = journal
                    .lock()
                    .map_err(|_| error("permission journal is poisoned"))?;
                if !self.established {
                    journal.wait_admission_exit(deadline).map_err(error)?;
                }
                journal.wait_idle(deadline).map_err(error)
            },
            || {
                prepare
                    .as_mut()
                    .map_or(Ok(()), NativePrepare::restore)
                    .map_err(|failure| {
                        error(format!(
                            "restoring backend workspace permissions: {failure:#}"
                        ))
                    })
            },
            || {
                journal
                    .lock()
                    .map_err(|_| error("permission journal is poisoned"))?
                    .finish(deadline)
                    .map_err(error)
            },
            || fs4::FileExt::unlock(lease).map_err(error),
        )
    }
}

impl Connection for NativeConnection {
    fn call(&mut self, mut operation: Operation, deadline: Deadline) -> Result<Response> {
        if self.cleanup == CleanupPhase::Complete || self.poisoned {
            return Err(error("private backend connection is unavailable").before_dispatch());
        }
        if let Operation::Start(spec) = &mut operation
            && let Some(cwd) = &spec.cwd
        {
            // Resolve against the owner's cwd; the worker has only search access to that base.
            spec.cwd = Some(self.base.join(cwd).canonicalize().map_err(|failure| {
                error(format!(
                    "requested working directory cannot be resolved: {failure}"
                ))
                .before_dispatch()
            })?);
        }
        let prepare = self
            .prepare
            .as_mut()
            .ok_or_else(|| error("private backend was not initialized").before_dispatch())?;
        match &mut operation {
            Operation::Configure(paths) => {
                let root = self
                    .journal
                    .lock()
                    .map_err(|_| error("permission journal is poisoned"))?
                    .protected_root()
                    .to_path_buf();
                if !paths.iter().any(|path| path.path == root) {
                    paths.push(glass_core::ProtectedHostPath::directory(root));
                }
                match prepare.protect(paths) {
                    Ok(()) => {}
                    Err(PreparationError::Refused(refused)) => {
                        return Err(error(refused).before_dispatch());
                    }
                    Err(PreparationError::CleanupUnconfirmed(failed)) => {
                        self.poisoned = true;
                        return Err(error(failed));
                    }
                }
            }
            Operation::Start(spec) => match prepare.prepare_spec(spec) {
                Ok(()) => {}
                Err(PreparationError::Refused(refused)) => {
                    return Err(error(refused).before_dispatch());
                }
                Err(PreparationError::CleanupUnconfirmed(failed)) => {
                    self.poisoned = true;
                    return Err(error(failed));
                }
            },
            _ => {}
        }
        self.sequence += 1;
        let request = Request {
            version: crate::backend_worker::VERSION,
            id: self.sequence,
            expires_ns: crate::backend_worker::expires(deadline)?,
            operation,
        };
        let own = if matches!(request.operation, Operation::Start(_)) {
            None
        } else {
            Some(Duration::from_secs(20))
        };
        let io_deadline = match own {
            Some(duration) => {
                Deadline::at(deadline.resolve(std::time::Instant::now() + duration).0)
            }
            None => deadline,
        };
        let mut bytes = serde_json::to_vec(&request).map_err(error)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_MESSAGE {
            return Err(error("backend request exceeds message limit").before_dispatch());
        }
        let outcome = (|| {
            for chunk in bytes.chunks(CHUNK) {
                send(&mut self.stream, DATA, chunk, io_deadline, None)?;
            }
            loop {
                if let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
                    let remainder = self.buffer.split_off(end + 1);
                    let reply: Response =
                        serde_json::from_slice(&self.buffer[..end]).map_err(error)?;
                    self.buffer = remainder;
                    if reply.version != crate::backend_worker::VERSION || reply.id != request.id {
                        return Err(error("backend reply does not match its request"));
                    }
                    return Ok(reply);
                }
                match receive(&mut self.stream, io_deadline, None)? {
                    (DATA, data) => {
                        if self.buffer.len().saturating_add(data.len()) > MAX_MESSAGE {
                            return Err(error("backend response exceeds message limit"));
                        }
                        self.buffer.extend(data);
                    }
                    (EXIT, data) => {
                        self.receipt(&data)?;
                        return Err(error("backend worker exited before replying"));
                    }
                    _ => return Err(error("unexpected backend worker frame")),
                }
            }
        })();
        if outcome.is_err() {
            self.poisoned = true;
            if deadline.has_passed() {
                return Err(GlassError::caller_deadline_elapsed(
                    "backend worker operation",
                ));
            }
        }
        outcome.map_err(GlassError::after_dispatch)
    }

    fn cleanup_complete(&self) -> bool {
        self.cleanup == CleanupPhase::Complete
    }

    fn close(&mut self, deadline: Deadline) -> Result<()> {
        let deadline = bounded(deadline, Some(Duration::from_secs(8)));
        if self.cleanup == CleanupPhase::Complete {
            return self
                .receipt_failure
                .as_ref()
                .map_or(Ok(()), |failure| Err(error(failure)));
        }
        let receipt = if self.exit_received {
            self.receipt_failure
                .as_ref()
                .map_or(Ok(()), |failure| Err(error(failure)))
        } else {
            let _ = send(&mut self.stream, CLOSE, &[], deadline, None);
            let _ = self.stream.shutdown(Shutdown::Write);
            (|| {
                loop {
                    match receive(&mut self.stream, deadline, None)? {
                        (EXIT, data) => return self.receipt(&data),
                        (DATA, _) => {}
                        _ => return Err(error("unexpected frame during backend cleanup")),
                    }
                }
            })()
        };
        let restored = self.restore_permissions(deadline);
        if self.cleanup == CleanupPhase::Complete
            && let Err(failure) = &receipt
        {
            self.receipt_failure = Some(failure.to_string());
        }
        exit_and_cleanup(receipt, restored)
    }
}

impl Drop for NativeConnection {
    fn drop(&mut self) {
        let result = if self.cleanup == CleanupPhase::Complete {
            Ok(())
        } else if self.established {
            self.close(Deadline::from_millis(1500))
        } else {
            self.abort_connect(Deadline::from_millis(1500))
        };
        if let Err(error) = result {
            eprintln!("glass: private worker/workspace cleanup unconfirmed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs a signed, enrolled worker in a background graphical login"]
    fn consecutive_readiness_checks_allow_launchd_to_relaunch_the_worker() {
        assert!(glass_macos::private_session::configured().unwrap());
        assert!(glass_macos::private_session::is_owner());
        for index in 0..3 {
            let started = std::time::Instant::now();
            let facts = readiness().expect("authenticate a fresh worker after the previous probe");
            eprintln!(
                "readiness check {index} completed in {:?}",
                started.elapsed()
            );
            assert!(facts.private_session && facts.accessibility && facts.screen_recording);
        }
    }

    #[test]
    fn failed_construction_closes_even_an_empty_journal_and_releases_the_account_lease() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = crate::permission_journal::PermissionJournal::open(
            root.path(),
            u32::MAX,
            "test-account".into(),
            None,
            None,
        )
        .unwrap();
        let lease_file = tempfile::NamedTempFile::new().unwrap();
        let lease = File::open(lease_file.path()).unwrap();
        let observed_lease = File::open(lease_file.path()).unwrap();
        fs4::FileExt::try_lock(&lease).unwrap();
        assert!(fs4::FileExt::try_lock(&observed_lease).is_err());
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let connection = NativeConnection {
            _environment_lease: crate::macos_setup::Lease::new(tempfile::tempfile().unwrap()),
            stream,
            lease,
            prepare: None,
            established: false,
            base: PathBuf::new(),
            buffer: Vec::new(),
            sequence: 0,
            clean_exit: false,
            exit_received: false,
            cleanup: CleanupPhase::default(),
            receipt_failure: None,
            poisoned: false,
            journal: Arc::new(Mutex::new(journal)),
        };
        let failed = connection.finish_connect(Err(error("native-backend spawn failed")));
        assert!(
            matches!(failed, Err(ref failure) if failure.to_string().contains("native-backend spawn failed"))
        );
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        let events = std::fs::read_to_string(root.path().join("pending.jsonl")).unwrap();
        assert!(events.lines().last().unwrap().contains("Closed"));
        fs4::FileExt::try_lock(&observed_lease).unwrap();
        let reopened = crate::permission_journal::PermissionJournal::open(
            root.path(),
            u32::MAX,
            "test-account".into(),
            None,
            None,
        )
        .unwrap();
        drop(reopened);
    }

    #[test]
    fn queued_exit_is_read_after_both_sides_close_their_input() {
        let (mut owner, mut worker) = UnixStream::pair().unwrap();
        owner.set_nonblocking(true).unwrap();
        worker.set_nonblocking(true).unwrap();
        let deadline = Deadline::from_millis(1000);
        send(&mut worker, EXIT, b"receipt", deadline, None).unwrap();
        worker.shutdown(Shutdown::Write).unwrap();
        owner.shutdown(Shutdown::Write).unwrap();
        drop(worker);
        assert_eq!(
            receive(&mut owner, deadline, None).unwrap(),
            (EXIT, b"receipt".to_vec())
        );
    }

    #[test]
    fn partial_header_cannot_restart_the_caller_budget() {
        let (mut owner, mut worker) = UnixStream::pair().unwrap();
        owner.set_nonblocking(true).unwrap();
        worker.write_all(&[DATA]).unwrap();
        let start = std::time::Instant::now();
        let result = receive(&mut owner, Deadline::from_millis(30), None);
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn a_bad_exit_does_not_skip_cleanup_or_become_a_clean_exit() {
        let mut phase = CleanupPhase::default();
        let calls = std::cell::Cell::new(0);
        let step = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let result = exit_and_cleanup(
            Err(error("abnormal receipt")),
            phase.advance(step, step, step, step),
        );
        assert!(result.unwrap_err().to_string().contains("abnormal receipt"));
        assert_eq!(calls.get(), 4);
        assert_eq!(phase, CleanupPhase::Complete);
    }

    #[test]
    fn live_work_prevents_acl_restore_and_preserves_the_receipt_error() {
        let mut phase = CleanupPhase::default();
        let cleanup = phase.advance(
            || Err(error("work still alive")),
            || panic!("do not revoke"),
            || panic!("do not close journal"),
            || panic!("do not unlock"),
        );
        let result = exit_and_cleanup(Err(error("bad receipt")), cleanup).unwrap_err();
        assert!(result.to_string().contains("bad receipt"));
        assert!(result.to_string().contains("work still alive"));
        assert_eq!(phase, CleanupPhase::WaitingForIdle);
    }

    #[test]
    fn cleanup_retry_does_not_repeat_restored_scopes_or_journal_closure() {
        let mut phase = CleanupPhase::default();
        assert!(
            phase
                .advance(
                    || Ok(()),
                    || Ok(()),
                    || Err(error("journal sync failed")),
                    || panic!("no unlock")
                )
                .is_err()
        );
        assert_eq!(phase, CleanupPhase::AccessRestored);
        assert!(
            phase
                .advance(
                    || panic!("already idle"),
                    || panic!("already restored"),
                    || Ok(()),
                    || Err(error("unlock failed"))
                )
                .is_err()
        );
        assert_eq!(phase, CleanupPhase::JournalFinished);
        phase
            .advance(
                || panic!("already idle"),
                || panic!("already restored"),
                || panic!("already journaled"),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(phase, CleanupPhase::Complete);
    }
}
