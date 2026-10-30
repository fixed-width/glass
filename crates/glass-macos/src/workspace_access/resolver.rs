//! Bounded, read-only pathname lookup; reopening and ACL writes stay with the caller.

use std::io;
use std::path::PathBuf;
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use super::FileIdentity;

type Answer = io::Result<Option<PathBuf>>;
struct Request {
    identity: FileIdentity,
    expires: Instant,
    reply: mpsc::Sender<Answer>,
}

struct Resolver(mpsc::SyncSender<Request>);

fn timeout() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "filesystem identity lookup timed out",
    )
}

impl Resolver {
    fn new(query: impl Fn(FileIdentity) -> Answer + Send + 'static) -> io::Result<Self> {
        let (send, receive) = mpsc::sync_channel::<Request>(64);
        std::thread::Builder::new()
            .name("glass-file-identity".into())
            .spawn(move || {
                for request in receive {
                    let answer = if Instant::now() >= request.expires {
                        Err(timeout())
                    } else {
                        query(request.identity)
                    };
                    let _ = request.reply.send(answer);
                }
            })?;
        Ok(Self(send))
    }

    fn resolve(&self, identity: FileIdentity, wait: Duration) -> Answer {
        let (reply, receive) = mpsc::channel();
        self.0
            .try_send(Request {
                identity,
                expires: Instant::now() + wait,
                reply,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem identity lookup is busy",
                ),
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::other("filesystem identity resolver exited")
                }
            })?;
        receive.recv_timeout(wait).map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => timeout(),
            mpsc::RecvTimeoutError::Disconnected => {
                io::Error::other("filesystem identity resolver exited")
            }
        })?
    }
}

pub(super) fn resolve(identity: FileIdentity) -> Answer {
    static RESOLVER: OnceLock<io::Result<Resolver>> = OnceLock::new();
    RESOLVER
        .get_or_init(|| Resolver::new(query))
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))?
        .resolve(identity, Duration::from_secs(2))
}

fn query(identity: FileIdentity) -> Answer {
    unsafe extern "C" {
        fn glass_private_resolve_file(
            identity: *const FileIdentity,
            out: *mut u8,
            size: usize,
        ) -> std::ffi::c_int;
    }
    let mut path = [0u8; 4096];
    // SAFETY: the helper borrows a live repr(C) identity and writes within this buffer.
    let code = unsafe { glass_private_resolve_file(&identity, path.as_mut_ptr(), path.len()) };
    if code == 2 {
        return Ok(None);
    }
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    let end = path.iter().position(|&byte| byte == 0).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "recovered file path is unterminated",
        )
    })?;
    use std::os::unix::ffi::OsStrExt;
    Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(
        &path[..end],
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_volume_selection_never_opens_unrelated_mount_roots() {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let temporary = tempfile::tempdir_in("/private/tmp").unwrap();
        let binary = temporary.path().join("volume-lookup");
        let fixture = temporary.path().join("identity");
        std::fs::write(&fixture, b"owned").unwrap();
        let output = std::process::Command::new("/usr/bin/xcrun")
            .args(["clang", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg("-I")
            .arg(source.join("src/workspace_access"))
            .arg(source.join("tests/volume_lookup.m"))
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new(binary)
            .arg(fixture)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn blocked_lookup_has_bounded_queue_and_recovers_without_late_queries() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (entered, observe) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let resolver = Resolver::new(move |_| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
            }
            Ok(Some(PathBuf::from("/observed")))
        })
        .unwrap();
        let (reply, first) = mpsc::channel();
        resolver
            .0
            .try_send(Request {
                identity: FileIdentity::default(),
                expires: Instant::now() + Duration::from_secs(10),
                reply,
            })
            .unwrap();
        observe.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            resolver
                .resolve(FileIdentity::default(), Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let mut waiting = Vec::new();
        // The timed-out request occupies one queue slot until the metadata call returns.
        for _ in 0..63 {
            let (reply, receive) = mpsc::channel();
            resolver
                .0
                .try_send(Request {
                    identity: FileIdentity::default(),
                    expires: Instant::now(),
                    reply,
                })
                .unwrap();
            waiting.push(receive);
        }
        assert_eq!(
            resolver
                .resolve(FileIdentity::default(), Duration::from_secs(2))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        release.send(()).unwrap();
        assert_eq!(
            first.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(),
            Some(PathBuf::from("/observed"))
        );
        for response in waiting {
            assert_eq!(
                response
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::TimedOut
            );
        }
        assert!(observe.try_recv().is_err());
        assert_eq!(
            resolver
                .resolve(FileIdentity::default(), Duration::from_secs(2))
                .unwrap(),
            Some(PathBuf::from("/observed"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
