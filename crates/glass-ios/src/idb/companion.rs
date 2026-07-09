//! Spawns and owns the `idb_companion` process bound to one simulator UDID, and
//! exposes the Unix socket it serves gRPC on. Killing the child on Drop reaps it
//! (Child::drop does NOT kill), mirroring glass-android's AgentRegistry lifetime.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use glass_core::{GlassError, Result};

/// `GLASS_IDB_COMPANION`, else `idb_companion` on PATH.
pub fn companion_bin(get: &dyn Fn(&str) -> Option<String>) -> String {
    get("GLASS_IDB_COMPANION")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "idb_companion".to_string())
}

/// The Unix-domain socket path the companion is told to serve on, under `dir`.
/// Kept deliberately short: macOS caps `sun_path` at 104 bytes and its per-user temp
/// dir (`/var/folders/…/T/`) already spends ~50 of them, so a longer name makes the
/// companion refuse to bind (`unixDomainSocketPathTooLong`). The file name therefore
/// carries only a UDID prefix — enough to tell simulators apart when debugging — plus
/// this process's pid. Uniqueness rests on the pid (one companion per process), so a
/// same-process re-spawn reuses the path and the pre-spawn `remove_file` self-heals it.
fn socket_path(dir: &Path, udid: &str, pid: u32) -> PathBuf {
    let udid_prefix: String = udid.chars().take(8).collect();
    dir.join(format!("glass-idb-{udid_prefix}-{pid}.sock"))
}

/// Owns one `idb_companion` child process and the Unix socket it serves gRPC
/// on. Killing + reaping the child on `Drop` mirrors glass-android's
/// `AgentRegistry`/`AgentProc`.
pub struct IdbCompanion {
    child: Child,
    sock: PathBuf,
}

impl IdbCompanion {
    /// Spawn `idb_companion` bound to `udid`, and block until its gRPC socket
    /// is accepting connections (or return a `Backend` error). A failed spawn
    /// or a socket that never comes up leaves no child behind: both paths
    /// kill + reap before returning `Err`.
    pub fn spawn(udid: &str) -> Result<IdbCompanion> {
        let get = |k: &str| std::env::var(k).ok();
        let bin = companion_bin(&get);
        let sock = socket_path(&std::env::temp_dir(), udid, std::process::id());
        let _ = std::fs::remove_file(&sock);
        let child = Command::new(&bin)
            .args(["--udid", udid, "--grpc-domain-sock"])
            .arg(&sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| {
                GlassError::Backend(format!(
                    "spawn {bin}: {e} (install: brew install idb-companion)"
                ))
            })?;
        let mut this = IdbCompanion { child, sock };
        // From here any failure must kill+reap the child, so a failed spawn never leaks it.
        if let Err(e) = wait_for_socket(&this.sock, Instant::now() + Duration::from_secs(10)) {
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

    /// A stub companion for `IosPlatform` unit tests that build a platform without a
    /// real `idb_companion`. The child is a trivial process (so `Drop` has something to
    /// reap) and the socket path is a placeholder these tests never connect to.
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
        }
    }
}

impl Drop for IdbCompanion {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// Poll until `idb_companion`'s gRPC socket accepts a connection, or `deadline`.
/// A raw Unix-domain connect is near-instant: it accepts once the companion is
/// serving (it binds its gRPC server to the socket before accepting) and returns
/// ECONNREFUSED immediately if the socket file exists but nothing is listening yet.
/// That keeps each poll attempt bounded so the outer `deadline` is respected; the
/// first real RPC carries its own timeout as a backstop.
fn wait_for_socket(sock: &Path, deadline: Instant) -> Result<()> {
    loop {
        if std::os::unix::net::UnixStream::connect(sock).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(GlassError::Backend(format!(
                "idb_companion never opened its socket at {}",
                sock.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
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
        // built path must stay under the cap even with a full 36-char simulator UDID and a
        // large pid. This is the exact shape `spawn` passes to `--grpc-domain-sock`.
        let dir = Path::new("/var/folders/2g/t424cmtn67j0txp_3hj67k980000gn/T");
        let p = socket_path(dir, "42C037FF-28A3-415E-BBCB-B2A17004E566", u32::MAX);
        assert!(
            p.as_os_str().len() <= 104,
            "socket path {} is {} bytes, over the 104-byte sun_path limit",
            p.display(),
            p.as_os_str().len()
        );
    }

    #[test]
    fn wait_for_socket_times_out_when_absent() {
        use std::time::{Duration, Instant};
        let missing = std::path::Path::new("/nonexistent/never.sock");
        let err =
            wait_for_socket(missing, Instant::now() + Duration::from_millis(150)).unwrap_err();
        assert!(matches!(err, glass_core::GlassError::Backend(_)), "{err:?}");
    }

    #[test]
    fn wait_for_socket_returns_ok_when_listening() {
        use std::time::{Duration, Instant};
        // A bound listener is enough: the kernel accepts the connect into its backlog,
        // so the raw UDS probe succeeds without an accept thread. Proves the ready path
        // on Linux, with no real idb_companion.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("idb.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        wait_for_socket(&sock, Instant::now() + Duration::from_secs(2)).expect("socket ready");
    }
}
