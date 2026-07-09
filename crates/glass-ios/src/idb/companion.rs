//! Spawns and owns the `idb_companion` process bound to one simulator UDID, and
//! exposes the Unix socket it serves gRPC on. Killing the child on Drop reaps it
//! (Child::drop does NOT kill), mirroring glass-android's AgentRegistry lifetime.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use glass_core::{GlassError, Result};

use super::client::IdbClient;

// A later increment wires `IosPlatform` to own an `IdbCompanion` (spawning it once a
// simulator UDID is resolved); until then nothing in-crate calls this beyond its own
// tests, and the `idb` module is crate-private, so `pub` alone does not exempt it from
// `dead_code`.
#[allow(dead_code)]
/// `GLASS_IDB_COMPANION`, else `idb_companion` on PATH.
pub fn companion_bin(get: &dyn Fn(&str) -> Option<String>) -> String {
    get("GLASS_IDB_COMPANION")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "idb_companion".to_string())
}

/// Owns one `idb_companion` child process and the Unix socket it serves gRPC
/// on. Killing + reaping the child on `Drop` mirrors glass-android's
/// `AgentRegistry`/`AgentProc`.
#[allow(dead_code)]
pub struct IdbCompanion {
    child: Child,
    sock: PathBuf,
}

#[allow(dead_code)]
impl IdbCompanion {
    /// Spawn `idb_companion` bound to `udid`, and block until its gRPC socket
    /// is accepting connections (or return a `Backend` error). A failed spawn
    /// or a socket that never comes up leaves no child behind: both paths
    /// kill + reap before returning `Err`.
    pub fn spawn(udid: &str) -> Result<IdbCompanion> {
        let get = |k: &str| std::env::var(k).ok();
        let bin = companion_bin(&get);
        // A per-companion socket under the temp dir; unique by pid to avoid collisions.
        let sock =
            std::env::temp_dir().join(format!("glass-idb-{udid}-{}.sock", std::process::id()));
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
}

impl Drop for IdbCompanion {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// Poll until `idb_companion`'s gRPC socket accepts a connection, or `deadline`.
/// A successful [`IdbClient::connect`] means the gRPC server is up, since the
/// connector does the h2 handshake eagerly.
#[allow(dead_code)] // only called from `spawn`, itself unused until a later increment
fn wait_for_socket(sock: &Path, deadline: Instant) -> Result<()> {
    loop {
        if sock.exists() && IdbClient::connect(sock).is_ok() {
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
    fn wait_for_socket_times_out_when_absent() {
        use std::time::{Duration, Instant};
        let missing = std::path::Path::new("/nonexistent/never.sock");
        let err =
            wait_for_socket(missing, Instant::now() + Duration::from_millis(150)).unwrap_err();
        assert!(matches!(err, glass_core::GlassError::Backend(_)), "{err:?}");
    }
}
