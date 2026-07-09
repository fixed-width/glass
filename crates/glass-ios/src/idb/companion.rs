//! Spawns and owns the `idb_companion` process bound to one simulator UDID, and
//! exposes the Unix socket it serves gRPC on. Killing the child on Drop reaps it
//! (Child::drop does NOT kill), mirroring glass-android's AgentRegistry lifetime.
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use glass_core::{GlassError, Result};

/// macOS caps a Unix-domain socket path (`sun_path`) at 104 bytes; a longer one makes the
/// companion refuse to bind with an opaque `unixDomainSocketPathTooLong`.
const SUN_PATH_MAX: usize = 104;
/// How much of the UDID goes into the socket file name — enough to tell simulators apart
/// at a glance while leaving room under [`SUN_PATH_MAX`] for the long macOS temp dir.
const SOCK_UDID_PREFIX_LEN: usize = 16;
/// Poll interval while waiting for the companion's socket to come up.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How long to wait for the companion to open its socket before giving up.
const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// `GLASS_IDB_COMPANION`, else `idb_companion` on PATH.
pub fn companion_bin(get: &dyn Fn(&str) -> Option<String>) -> String {
    get("GLASS_IDB_COMPANION")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "idb_companion".to_string())
}

/// The Unix-domain socket path the companion is told to serve on, under `dir`.
/// Kept deliberately short: macOS caps `sun_path` at [`SUN_PATH_MAX`] bytes and its
/// per-user temp dir (`/var/folders/…/T/`) already spends ~50 of them, so a longer name
/// makes the companion refuse to bind (`unixDomainSocketPathTooLong`). The file name
/// therefore carries only a [`SOCK_UDID_PREFIX_LEN`]-char UDID prefix — enough to tell
/// simulators apart when debugging — plus this process's pid. Uniqueness rests on the pid
/// (one companion per process), so a same-process re-spawn reuses the path and the
/// pre-spawn `remove_file` self-heals it.
fn socket_path(dir: &Path, udid: &str, pid: u32) -> PathBuf {
    let udid_prefix: String = udid.chars().take(SOCK_UDID_PREFIX_LEN).collect();
    dir.join(format!("glass-idb-{udid_prefix}-{pid}.sock"))
}

/// Where the companion's stderr is captured, under `dir`. A sibling of the socket, keyed
/// the same way; unlike the socket it is a regular file (no `sun_path` limit), so its
/// startup failures can be read back into the returned error.
fn stderr_log_path(dir: &Path, udid: &str, pid: u32) -> PathBuf {
    let udid_prefix: String = udid.chars().take(SOCK_UDID_PREFIX_LEN).collect();
    dir.join(format!("glass-idb-{udid_prefix}-{pid}.stderr.log"))
}

/// Owns one `idb_companion` child process and the Unix socket it serves gRPC
/// on. Killing + reaping the child on `Drop` mirrors glass-android's
/// `AgentRegistry`/`AgentProc`.
pub struct IdbCompanion {
    child: Child,
    sock: PathBuf,
    /// File the companion's stderr is redirected to, read back on a startup failure so the
    /// error names the real cause; removed on `Drop`.
    stderr_log: PathBuf,
}

impl IdbCompanion {
    /// Spawn `idb_companion` bound to `udid`, and block until its gRPC socket
    /// is accepting connections (or return a `Backend` error). A failed spawn
    /// or a socket that never comes up leaves no child behind: both paths
    /// kill + reap before returning `Err`.
    pub fn spawn(udid: &str) -> Result<IdbCompanion> {
        let get = |k: &str| std::env::var(k).ok();
        let bin = companion_bin(&get);
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let sock = socket_path(&dir, udid, pid);
        // Guard the socket-path length up front: idb refuses to bind an over-long
        // `sun_path` and does so opaquely, so name the real cause here rather than letting
        // it collapse to a generic bind failure downstream.
        if sock.as_os_str().len() > SUN_PATH_MAX {
            return Err(GlassError::Backend(format!(
                "idb_companion socket path too long ({} bytes > {SUN_PATH_MAX}-byte sun_path limit): {}",
                sock.as_os_str().len(),
                sock.display()
            )));
        }
        let _ = std::fs::remove_file(&sock);

        // Capture the companion's stderr to a file (not a pipe): a file can't fill and
        // block the long-lived companion when nothing is draining it, and it can be read
        // back if startup fails. Redirected for the child's whole life; removed on `Drop`.
        let stderr_log = stderr_log_path(&dir, udid, pid);
        let _ = std::fs::remove_file(&stderr_log);
        let stderr_file = std::fs::File::create(&stderr_log).map_err(|e| {
            GlassError::Backend(format!(
                "idb_companion: create stderr log {}: {e}",
                stderr_log.display()
            ))
        })?;

        let child = Command::new(&bin)
            .args(["--udid", udid, "--grpc-domain-sock"])
            .arg(&sock)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| {
                let _ = std::fs::remove_file(&stderr_log);
                GlassError::Backend(format!(
                    "spawn {bin}: {e} (install: brew install idb-companion)"
                ))
            })?;
        let mut this = IdbCompanion {
            child,
            sock,
            stderr_log,
        };
        // From here any failure must kill+reap the child, so a failed spawn never leaks it.
        if let Err(e) = this.await_socket(Instant::now() + SOCKET_READY_TIMEOUT) {
            let _ = this.child.kill();
            let _ = this.child.wait();
            return Err(e);
        }
        Ok(this)
    }

    /// The Unix socket `idb_companion` serves gRPC on.
    pub fn socket(&self) -> &Path {
        &self.sock
    }

    /// Block until the companion's gRPC socket accepts a connection, or `deadline`. Each
    /// iteration first checks whether the child already exited: a companion that dies on
    /// startup (bad UDID, unbootable sim, incompatible idb) is reported *immediately* with
    /// its captured stderr, instead of waiting out the full deadline on a socket that will
    /// never open.
    fn await_socket(&mut self, deadline: Instant) -> Result<()> {
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| GlassError::Backend(format!("idb_companion: try_wait: {e}")))?
            {
                return Err(GlassError::Backend(format!(
                    "idb_companion exited ({status}) before serving its socket{}",
                    self.stderr_suffix()
                )));
            }
            if socket_ready(&self.sock) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(GlassError::Backend(format!(
                    "idb_companion never opened its socket at {}{}",
                    self.sock.display(),
                    self.stderr_suffix()
                )));
            }
            std::thread::sleep(SOCKET_POLL_INTERVAL);
        }
    }

    /// The companion's captured stderr, formatted as a `: <stderr>` suffix for an error
    /// message — or empty when it wrote nothing (so the message reads cleanly either way).
    fn stderr_suffix(&self) -> String {
        let err = std::fs::read_to_string(&self.stderr_log)
            .unwrap_or_default()
            .trim()
            .to_string();
        if err.is_empty() {
            String::new()
        } else {
            format!(": {err}")
        }
    }

    /// A stub companion for `IosPlatform` unit tests that build a platform without a
    /// real `idb_companion`. The child is a trivial process (so `Drop` has something to
    /// reap) and the paths are placeholders these tests never connect to or read.
    #[cfg(test)]
    pub(crate) fn for_test() -> IdbCompanion {
        let child = Command::new("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn `true` for a stub test companion");
        IdbCompanion {
            child,
            sock: PathBuf::from("/nonexistent/glass-idb-test.sock"),
            stderr_log: PathBuf::from("/nonexistent/glass-idb-test.stderr.log"),
        }
    }
}

impl Drop for IdbCompanion {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock);
        let _ = std::fs::remove_file(&self.stderr_log);
    }
}

/// One attempt to connect to `idb_companion`'s gRPC socket: `true` once it accepts.
/// A raw Unix-domain connect is near-instant — it accepts once the companion is serving
/// (it binds its gRPC server to the socket before accepting) and returns ECONNREFUSED
/// immediately if the socket file exists but nothing is listening yet — so polling it
/// keeps [`IdbCompanion::await_socket`]'s deadline responsive; the first real RPC carries
/// its own timeout as a backstop.
fn socket_ready(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn companion_bin_prefers_env_then_default() {
        let with =
            |m: HashMap<&'static str, &'static str>| move |k: &str| m.get(k).map(|s| s.to_string());
        assert_eq!(
            companion_bin(&with(HashMap::from([(
                "GLASS_IDB_COMPANION",
                "/opt/idb_companion"
            )]))),
            "/opt/idb_companion"
        );
        assert_eq!(companion_bin(&with(HashMap::new())), "idb_companion");
    }

    #[test]
    fn socket_path_stays_within_the_macos_sun_path_limit() {
        // macOS's per-user temp dir is long and `sun_path` caps at 104 bytes; a longer
        // path makes idb_companion refuse to bind (unixDomainSocketPathTooLong), so the
        // built path must stay under the cap even with a full 36-char simulator UDID (only
        // its SOCK_UDID_PREFIX_LEN-char prefix is used) and a large pid. This is the exact
        // shape `spawn` passes to `--grpc-domain-sock`.
        let dir = Path::new("/var/folders/2g/t424cmtn67j0txp_3hj67k980000gn/T");
        let p = socket_path(dir, "42C037FF-28A3-415E-BBCB-B2A17004E566", u32::MAX);
        assert!(
            p.as_os_str().len() <= SUN_PATH_MAX,
            "socket path {} is {} bytes, over the {SUN_PATH_MAX}-byte sun_path limit",
            p.display(),
            p.as_os_str().len()
        );
    }

    #[test]
    fn socket_ready_is_false_when_socket_is_absent() {
        assert!(!socket_ready(std::path::Path::new(
            "/nonexistent/never.sock"
        )));
    }

    #[test]
    fn socket_ready_is_true_when_listening() {
        // A bound listener is enough: the kernel accepts the connect into its backlog,
        // so the raw UDS probe succeeds without an accept thread. Proves the ready path
        // on Linux, with no real idb_companion.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("idb.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        assert!(socket_ready(&sock));
    }
}
