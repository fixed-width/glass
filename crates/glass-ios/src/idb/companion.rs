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

/// Standard Homebrew `idb_companion` locations, probed when it is not on `PATH`. A `.app` or
/// LaunchAgent glass is launched by launchd with a minimal `PATH` that omits Homebrew's bindir,
/// so a `brew install idb-companion` would otherwise be invisible and input/accessibility would
/// go dark with no way for the user to fix it short of setting an env var. Apple-silicon prefix
/// first, then Intel.
const HOMEBREW_COMPANION_PATHS: [&str; 2] = [
    "/opt/homebrew/bin/idb_companion",
    "/usr/local/bin/idb_companion",
];

/// The program to hand `Command::new` for the companion spawn. `GLASS_IDB_COMPANION` wins
/// verbatim (an explicit override is trusted — a wrong path surfaces a clear spawn error);
/// otherwise `idb_companion` is auto-discovered on `PATH`, then in Homebrew's standard prefixes;
/// failing all that, the bare name is returned so the spawn fails with the actionable
/// "install idb_companion" error rather than a silent no-op.
pub fn companion_bin(get: &dyn Fn(&str) -> Option<String>) -> String {
    companion_program(get, &|p| p.is_file())
}

/// [`companion_bin`] with the filesystem-existence check injected, so tests can drive resolution
/// without touching the real environment.
fn companion_program(
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> String {
    if let Some(explicit) = get("GLASS_IDB_COMPANION").filter(|s| !s.is_empty()) {
        return explicit;
    }
    discover_companion(get, exists)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "idb_companion".to_string())
}

/// Auto-discover `idb_companion` when `GLASS_IDB_COMPANION` is unset: on `PATH` first, then in
/// the standard Homebrew prefixes ([`HOMEBREW_COMPANION_PATHS`]). `None` if it is nowhere to be
/// found. `get`/`exists` are seams so tests drive it deterministically.
fn discover_companion(
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    on_path("idb_companion", get, exists).or_else(|| {
        HOMEBREW_COMPANION_PATHS
            .iter()
            .copied()
            .map(PathBuf::from)
            .find(|p| exists(p))
    })
}

/// The first `PATH` entry containing a file named `name`, if any.
fn on_path(
    name: &str,
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let path = get("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| exists(p))
}

/// Whether the companion resolves to an existing binary — the testable core of the doctor's
/// presence check, mirroring [`companion_bin`]'s resolution so the two never drift: an explicit
/// `GLASS_IDB_COMPANION` path must exist (a bare name must be on `PATH`); otherwise auto-discovery
/// (`PATH`, then Homebrew prefixes) must find one. `get`/`exists` are seams for tests.
pub(crate) fn companion_present_with(
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> bool {
    match get("GLASS_IDB_COMPANION").filter(|s| !s.is_empty()) {
        Some(bin) if bin.contains('/') => exists(Path::new(&bin)),
        Some(bin) => on_path(&bin, get, exists).is_some(),
        None => discover_companion(get, exists).is_some(),
    }
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
        Self::spawn_with(udid, &|k| std::env::var(k).ok(), SOCKET_READY_TIMEOUT)
    }

    /// [`spawn`](Self::spawn) with the companion-binary env resolution and the socket-ready
    /// deadline injected — the seam that makes the failure-cleanup path testable without a
    /// real simulator. A test points `GLASS_IDB_COMPANION` at a stub through `get_env`
    /// *without* mutating this process's environment (which would race parallel tests), and
    /// passes a short `ready_timeout` so a stub that never serves its socket fails fast.
    fn spawn_with(
        udid: &str,
        get_env: &dyn Fn(&str) -> Option<String>,
        ready_timeout: Duration,
    ) -> Result<IdbCompanion> {
        let bin = companion_bin(get_env);
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
        if let Err(e) = this.await_socket(Instant::now() + ready_timeout) {
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

    /// Build an env getter from a fixed set of pairs.
    fn env(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<&'static str, &'static str> = pairs.iter().copied().collect();
        move |k: &str| m.get(k).map(|s| s.to_string())
    }

    #[test]
    fn companion_program_uses_the_env_override_verbatim() {
        // An explicit override is trusted even when the file is absent — the spawn surfaces the error.
        assert_eq!(
            companion_program(
                &env(&[("GLASS_IDB_COMPANION", "/opt/idb_companion")]),
                &|_: &Path| false,
            ),
            "/opt/idb_companion"
        );
    }

    #[test]
    fn companion_program_falls_back_to_the_bare_name_when_nothing_resolves() {
        assert_eq!(
            companion_program(&env(&[("PATH", "/usr/bin:/bin")]), &|_: &Path| false),
            "idb_companion"
        );
    }

    #[test]
    fn discover_prefers_path_over_homebrew() {
        let exists = |p: &Path| {
            p == Path::new("/usr/bin/idb_companion")
                || p == Path::new("/opt/homebrew/bin/idb_companion")
        };
        assert_eq!(
            discover_companion(&env(&[("PATH", "/usr/bin")]), &exists),
            Some(PathBuf::from("/usr/bin/idb_companion"))
        );
    }

    #[test]
    fn discover_finds_homebrew_when_off_path() {
        // launchd's minimal PATH omits Homebrew's bindir; discovery still finds the brew install.
        let exists = |p: &Path| p == Path::new("/opt/homebrew/bin/idb_companion");
        assert_eq!(
            discover_companion(&env(&[("PATH", "/usr/bin:/bin")]), &exists),
            Some(PathBuf::from("/opt/homebrew/bin/idb_companion"))
        );
    }

    #[test]
    fn discover_prefers_apple_silicon_over_intel_prefix() {
        // Both prefixes "exist"; the arm64 prefix is chosen first.
        assert_eq!(
            discover_companion(&env(&[]), &|_: &Path| true),
            Some(PathBuf::from("/opt/homebrew/bin/idb_companion"))
        );
    }

    #[test]
    fn discover_is_none_when_absent_everywhere() {
        assert_eq!(
            discover_companion(&env(&[("PATH", "/usr/bin:/bin")]), &|_: &Path| false),
            None
        );
    }

    #[test]
    fn present_with_validates_that_an_override_path_exists() {
        let get = env(&[("GLASS_IDB_COMPANION", "/opt/idb_companion")]);
        assert!(companion_present_with(&get, &|p: &Path| p == Path::new("/opt/idb_companion")));
        assert!(!companion_present_with(&get, &|_: &Path| false));
    }

    #[test]
    fn present_with_resolves_a_bare_name_override_on_path() {
        let get = env(&[("GLASS_IDB_COMPANION", "my_idb"), ("PATH", "/usr/bin:/bin")]);
        assert!(companion_present_with(&get, &|p: &Path| p == Path::new("/bin/my_idb")));
    }

    #[test]
    fn present_with_finds_homebrew_when_off_path() {
        let get = env(&[("PATH", "/usr/bin:/bin")]);
        assert!(companion_present_with(&get, &|p: &Path| p
            == Path::new("/opt/homebrew/bin/idb_companion")));
    }

    #[test]
    fn present_with_is_false_when_absent_everywhere() {
        assert!(!companion_present_with(
            &env(&[("PATH", "/usr/bin")]),
            &|_: &Path| false
        ));
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

    #[test]
    fn spawn_reaps_the_child_when_the_socket_never_opens() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("stub_companion");
        let pidfile = dir.path().join("child.pid");
        // A stub that records its own pid, then sleeps without ever opening the gRPC socket,
        // so `await_socket` can only time out. `exec` preserves the pid, so the recorded pid
        // is exactly the child `spawn_with` owns and must kill+reap on the failure path.
        std::fs::write(
            &stub,
            format!("#!/bin/sh\necho $$ > {pidfile:?}\nexec sleep 10\n"),
        )
        .expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        // Point GLASS_IDB_COMPANION at the stub through the injected getter — never through
        // this process's real env, which would race parallel tests. A short ready deadline
        // keeps the never-served socket from costing the full production timeout.
        let stub_path = stub.to_str().expect("utf-8 stub path").to_string();
        let get_env = |k: &str| (k == "GLASS_IDB_COMPANION").then(|| stub_path.clone());
        const READY: Duration = Duration::from_millis(500);
        // A unique udid keeps this test's socket file name from colliding with another test
        // in the same process (the socket path is keyed on the udid prefix + this pid).
        const UDID: &str = "REAPTEST-0000-0000-0000-000000000000";

        // Retry past a transient ETXTBSY on the freshly-written stub: a sibling test thread's
        // fork can momentarily hold the write fd open, racing our exec. This affects only the
        // just-written fixture, never the installed idb_companion (same rationale as doctor).
        let mut err = None;
        for _ in 0..100 {
            match IdbCompanion::spawn_with(UDID, &get_env, READY) {
                Ok(_) => panic!("stub never opens a socket, so spawn_with must fail"),
                Err(GlassError::Backend(m)) if m.contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("spawn_with kept returning ETXTBSY after 100 retries");
        assert!(
            matches!(&err, GlassError::Backend(m) if m.contains("never opened its socket")),
            "expected a socket-timeout failure, got: {err:?}"
        );

        // The stub records its pid before sleeping, and reaching the timeout above means it
        // got that far; read it back, briefly tolerating a slow start under load.
        let start = Instant::now();
        let pid = loop {
            if let Ok(s) = std::fs::read_to_string(&pidfile) {
                let t = s.trim();
                if !t.is_empty() {
                    break t.to_string();
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "stub never recorded its pid"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        // The heart of the test: after the failure the child must be gone — killed AND reaped
        // — not left running or lingering as a zombie. `kill -0` succeeds for a live *or*
        // zombie pid and fails (ESRCH) only once it is fully reaped, so a still-signalable
        // pid means `spawn_with` leaked it.
        let alive = Command::new("kill")
            .args(["-0", &pid])
            // Silence its stderr: on the expected (reaped) path `kill -0` prints
            // "No such process", which would clutter passing-test output. The `.success()`
            // check below is what the assertion reads.
            .stderr(Stdio::null())
            .status()
            .expect("run kill -0")
            .success();
        assert!(
            !alive,
            "spawn_with leaked child pid {pid}: it was not killed+reaped on the failure path"
        );
    }
}
