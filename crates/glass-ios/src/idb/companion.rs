//! Spawns and owns the `idb_companion` process bound to one simulator UDID, and
//! exposes the Unix socket it serves gRPC on. Killing the child on Drop reaps it
//! (Child::drop does NOT kill), mirroring glass-android's AgentRegistry lifetime.
use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use glass_core::{GlassError, Result};
use glass_exec_unix::{Resolved, resolve_bin, resolve_path};

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

/// Standard Homebrew `idb_companion` locations, probed when `$PATH` yields no runnable copy. A `.app` or
/// LaunchAgent glass is launched by launchd with a minimal `PATH` that omits Homebrew's bindir,
/// so a `brew install idb-companion` would otherwise be invisible and input/accessibility would
/// go dark with no way for the user to fix it short of setting an env var. Apple-silicon prefix
/// first, then Intel.
const HOMEBREW_COMPANION_PATHS: [&str; 2] = [
    "/opt/homebrew/bin/idb_companion",
    "/usr/local/bin/idb_companion",
];

/// Env var naming the companion binary outright, bypassing discovery.
const COMPANION_ENV: &str = "GLASS_IDB_COMPANION";
/// The companion's program name, looked up on `$PATH` when nothing names it outright.
const COMPANION_BIN: &str = "idb_companion";
/// The install, tap first — `idb_companion` is not in Homebrew core.
pub(crate) const INSTALL_REMEDY: &str =
    "brew tap facebook/fb && brew trust facebook/fb && brew install idb-companion";

/// Why there is no companion to run. Two fields: the doctor prints cause and fix in separate
/// columns, the launch path joins them into one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoCompanion {
    pub(crate) cause: String,
    pub(crate) remedy: String,
}

impl NoCompanion {
    fn not_executable(p: &Path) -> NoCompanion {
        NoCompanion {
            cause: format!("idb_companion at {} is not executable", p.display()),
            remedy: "restore its execute bit (`chmod +x`), or point GLASS_IDB_COMPANION at a \
                     runnable binary"
                .into(),
        }
    }

    /// The companion is installed, but glass could not even stat the path (a prefix it cannot
    /// traverse). A permission to fix, not a package to install (glass#474).
    fn unreadable(p: &Path, e: &str) -> NoCompanion {
        NoCompanion {
            cause: format!(
                "idb_companion at {} could not be looked at ({e}) — it may be installed where \
                 glass cannot read it",
                p.display()
            ),
            remedy: "check that path's permissions, or point GLASS_IDB_COMPANION at a readable \
                     copy"
                .into(),
        }
    }

    /// Another install would not be looked at while the override stands.
    fn override_names_nothing() -> NoCompanion {
        NoCompanion {
            cause: "GLASS_IDB_COMPANION does not name a runnable idb_companion".into(),
            remedy: "point GLASS_IDB_COMPANION at a runnable idb_companion, or unset it to \
                     search PATH and Homebrew's standard prefixes"
                .into(),
        }
    }

    fn absent() -> NoCompanion {
        NoCompanion {
            cause: "idb_companion not found".into(),
            remedy: INSTALL_REMEDY.into(),
        }
    }

    /// A bare name is all `$PATH` can resolve, and there is no `$PATH`.
    fn override_needs_a_path() -> NoCompanion {
        NoCompanion {
            cause: "GLASS_IDB_COMPANION names a bare command, and PATH is unset in glass's \
                    environment"
                .into(),
            remedy: "set GLASS_IDB_COMPANION to an absolute path".into(),
        }
    }

    fn no_search_path() -> NoCompanion {
        NoCompanion {
            cause: "idb_companion could not be looked up — PATH is unset in glass's environment"
                .into(),
            remedy: format!(
                "set GLASS_IDB_COMPANION to its absolute path, give glass a PATH to search, or \
                 install it: {INSTALL_REMEDY}"
            ),
        }
    }

    /// Cause and fix in one string, for the launch path's single error message.
    fn message(&self) -> String {
        format!("{} — {}", self.cause, self.remedy)
    }
}

/// What glass knows about this host's `idb_companion`, from one read of the environment.
#[derive(Debug)]
pub(crate) struct CompanionFacts {
    /// This host's `idb_companion`, or why there is none.
    pub(crate) resolved: Resolved,
    /// `GLASS_IDB_COMPANION` named it outright, so discovery never ran — which changes what a
    /// resolution that found nothing says.
    pub(crate) override_set: bool,
}

impl CompanionFacts {
    /// Resolve from one read of the environment, so the two fields cannot describe different
    /// moments — `override_set` is true exactly when the resolution took the override branch.
    pub(crate) fn gather(get: &dyn Fn(&str) -> Option<OsString>) -> CompanionFacts {
        // An override set to "" is how a wrapper clears it, and a non-UTF-8 value cannot be a
        // program token `exec` would take; neither is an override.
        let over = get(COMPANION_ENV)
            .and_then(|v| v.into_string().ok())
            .filter(|s| !s.is_empty());
        CompanionFacts {
            override_set: over.is_some(),
            resolved: resolve_companion_with(over, get("PATH"), &HOMEBREW_COMPANION_PATHS),
        }
    }

    /// The binary to hand `Command::new`, or why there is none.
    ///
    /// A binary glass cannot run is refused here, not exec'd: `exec` comes back with a bare
    /// `EACCES` where the resolution already knows the file and the fix. The doctor renders the
    /// same [`NoCompanion`], so the preflight and the bring-up cannot disagree.
    pub(crate) fn spawn_target(&self) -> std::result::Result<&Path, NoCompanion> {
        match &self.resolved {
            Resolved::Found(p) => Ok(p),
            Resolved::NotExecutable(p) => Err(NoCompanion::not_executable(p)),
            // The companion may be installed where glass cannot even stat it — a permission, not
            // an install (glass#474).
            Resolved::Unreadable(p, e) => Err(NoCompanion::unreadable(p, e)),
            Resolved::Absent if self.override_set => Err(NoCompanion::override_names_nothing()),
            Resolved::Absent => Err(NoCompanion::absent()),
            Resolved::NoSearchPath if self.override_set => {
                Err(NoCompanion::override_needs_a_path())
            }
            Resolved::NoSearchPath => Err(NoCompanion::no_search_path()),
        }
    }
}

/// [`CompanionFacts::gather`]'s resolution against explicit inputs — the testable seam. The
/// Homebrew prefixes are injected too, so a test cannot be answered by the developer's own
/// `brew install`. `override_value` is already normalised: `Some` means an override is in force.
///
/// `GLASS_IDB_COMPANION` names the companion outright and is never searched for among
/// `fallbacks`, though a bare name still goes through `$PATH`; discovery walks `$PATH` first and
/// `fallbacks` only once it runs dry.
fn resolve_companion_with(
    override_value: Option<String>,
    path: Option<OsString>,
    fallbacks: &[&str],
) -> Resolved {
    if let Some(bin) = override_value {
        return resolve_bin(&bin, path.as_deref());
    }
    let mut first_unrunnable = None;
    let mut first_unreadable = None;
    let mut nothing_to_search = false;
    for resolved in std::iter::once(resolve_bin(COMPANION_BIN, path.as_deref()))
        .chain(fallbacks.iter().map(|c| resolve_path(Path::new(c))))
    {
        match resolved {
            Resolved::Found(p) => return Resolved::Found(p),
            Resolved::NotExecutable(p) => {
                first_unrunnable.get_or_insert(p);
            }
            // A prefix glass could not stat (glass#474): remember it, but a runnable or
            // present-but-unrunnable copy in a later entry still outranks it.
            Resolved::Unreadable(p, e) => {
                first_unreadable.get_or_insert((p, e));
            }
            // Only the `$PATH` walk can lack a search list; a fallback is one fixed path.
            Resolved::NoSearchPath => nothing_to_search = true,
            Resolved::Absent => {}
        }
    }
    match (first_unrunnable, first_unreadable, nothing_to_search) {
        // A file the user can act on outranks an unreadable prefix and the missing search list;
        // the remedy's second clause covers a copy that a `$PATH` would have found.
        (Some(p), _, _) => Resolved::NotExecutable(p),
        // No runnable/unrunnable file, but a prefix glass could not stat: name the permission
        // rather than the install (glass#474).
        (None, Some((p, e)), _) => Resolved::Unreadable(p, e),
        // No `$PATH` to walk and nothing in the standard prefixes: the companion may well be
        // installed somewhere glass was never told to look (glass#373).
        (None, None, true) => Resolved::NoSearchPath,
        (None, None, false) => Resolved::Absent,
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
        let facts = CompanionFacts::gather(&|k| std::env::var_os(k));
        let bin = facts
            .spawn_target()
            .map_err(|no| GlassError::Backend(no.message()))?;
        Self::spawn_bin(udid, bin)
    }

    /// [`spawn`](Self::spawn) against an already-resolved binary, for a caller holding one from
    /// [`resolve_companion`] that must not resolve again.
    pub(crate) fn spawn_bin(udid: &str, bin: &Path) -> Result<IdbCompanion> {
        Self::spawn_with(udid, bin, SOCKET_READY_TIMEOUT)
    }

    /// [`spawn_bin`](Self::spawn_bin) with the socket-ready deadline injected — the seam that
    /// makes the failure-cleanup path testable without a real simulator. A test points it at a
    /// stub and passes a short `ready_timeout`, so a stub that never serves its socket fails fast.
    fn spawn_with(udid: &str, bin: &Path, ready_timeout: Duration) -> Result<IdbCompanion> {
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

        let child = Command::new(bin)
            .args(["--udid", udid, "--grpc-domain-sock"])
            .arg(&sock)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| {
                let _ = std::fs::remove_file(&stderr_log);
                // Resolution asked `access(X_OK)`, which answers permission and not whether
                // the file is a loadable image — an x86_64 companion from the Intel prefix on an
                // arm64 Mac without Rosetta resolves and fails here — so this keeps a remedy.
                GlassError::Backend(format!(
                    "spawn {}: {e} — the binary resolved but did not start; check it matches \
                     this Mac's architecture, or reinstall it: {INSTALL_REMEDY}",
                    bin.display()
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
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;

    /// A directory holding `name` at `mode`. Real files at real modes, because resolution asks
    /// the kernel whether this process may execute one and no injected predicate stands in for
    /// that answer.
    fn dir_with(name: &str, mode: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join(name);
        std::fs::write(&bin, b"").expect("write");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).expect("chmod");
        dir
    }

    /// A `$PATH`-style value naming `dirs`.
    fn path_of(dirs: &[&Path]) -> Option<OsString> {
        Some(std::env::join_paths(dirs).expect("join paths"))
    }

    /// An env getter over a fixed set of pairs, in the shape [`CompanionFacts::gather`] reads.
    fn env<'a>(pairs: &'a [(&'a str, &'a OsStr)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_os_string())
        }
    }

    /// The `fallbacks` entry naming `dir`'s companion.
    fn candidate(dir: &tempfile::TempDir) -> String {
        dir.path()
            .join(COMPANION_BIN)
            .to_str()
            .expect("utf-8 temp path")
            .to_string()
    }

    #[test]
    fn a_runnable_companion_on_path_resolves_to_it() {
        let dir = dir_with(COMPANION_BIN, 0o755);
        assert_eq!(
            resolve_companion_with(None, path_of(&[dir.path()]), &[]),
            Resolved::Found(dir.path().join(COMPANION_BIN))
        );
    }

    /// glass#393. A companion that is installed and cannot be executed — no execute bit, a
    /// `noexec` mount, a permission class this user is not in — must not read as available:
    /// the spawn would fail, and "not found" sends the user to reinstall what is already there.
    #[test]
    fn a_companion_that_cannot_be_executed_is_reported_as_present_not_missing() {
        let dir = dir_with(COMPANION_BIN, 0o644);
        assert_eq!(
            resolve_companion_with(None, path_of(&[dir.path()]), &[]),
            Resolved::NotExecutable(dir.path().join(COMPANION_BIN))
        );
    }

    /// The same defect on the override path: an explicit `GLASS_IDB_COMPANION` is still a
    /// binary glass has to exec.
    #[test]
    fn an_override_naming_a_non_executable_binary_is_not_executable() {
        let dir = dir_with("my_idb", 0o644);
        let bin = dir.path().join("my_idb");
        assert_eq!(
            resolve_companion_with(
                Some(bin.to_str().expect("utf-8 temp path").to_string()),
                None,
                &[]
            ),
            Resolved::NotExecutable(bin)
        );
    }

    /// A runnable companion later in the order beats an unrunnable one earlier: resolving to the
    /// unrunnable one would spawn a binary the user's own shell would have walked past.
    #[test]
    fn a_non_executable_companion_on_path_does_not_shadow_a_runnable_homebrew_one() {
        let on_path = dir_with(COMPANION_BIN, 0o644);
        let brew = dir_with(COMPANION_BIN, 0o755);
        assert_eq!(
            resolve_companion_with(None, path_of(&[on_path.path()]), &[&candidate(&brew)]),
            Resolved::Found(brew.path().join(COMPANION_BIN))
        );
    }

    /// Nothing runnable anywhere: the first unrunnable file walked past is the one the user can
    /// act on, so it is reported rather than a flat "not found".
    #[test]
    fn with_nothing_runnable_the_first_unrunnable_companion_is_reported() {
        let on_path = dir_with(COMPANION_BIN, 0o644);
        let brew = dir_with(COMPANION_BIN, 0o600);
        assert_eq!(
            resolve_companion_with(None, path_of(&[on_path.path()]), &[&candidate(&brew)]),
            Resolved::NotExecutable(on_path.path().join(COMPANION_BIN))
        );
    }

    #[test]
    fn an_override_naming_a_runnable_binary_resolves_to_it() {
        let dir = dir_with("my_idb", 0o755);
        let bin = dir.path().join("my_idb");
        assert_eq!(
            resolve_companion_with(
                Some(bin.to_str().expect("utf-8 temp path").to_string()),
                None,
                &[]
            ),
            Resolved::Found(bin)
        );
    }

    #[test]
    fn a_bare_name_override_is_looked_up_on_path() {
        let dir = dir_with("my_idb", 0o755);
        assert_eq!(
            resolve_companion_with(Some("my_idb".to_string()), path_of(&[dir.path()]), &[]),
            Resolved::Found(dir.path().join("my_idb"))
        );
    }

    /// An override is taken at its word and never searched for among the Homebrew prefixes: a
    /// user who named a build wants that build, not whichever one `brew` installed.
    #[test]
    fn an_override_that_resolves_to_nothing_is_absent_even_when_homebrew_has_one() {
        let brew = dir_with(COMPANION_BIN, 0o755);
        assert_eq!(
            resolve_companion_with(
                Some("/nonexistent/my_idb".to_string()),
                None,
                &[&candidate(&brew)]
            ),
            Resolved::Absent
        );
    }

    /// An unset `GLASS_IDB_COMPANION` and one set to the empty string mean the same thing — and
    /// both fields have to say so, or the doctor blames a variable that decided nothing.
    #[test]
    fn an_empty_override_is_not_an_override() {
        let dir = dir_with(COMPANION_BIN, 0o755);
        let facts = CompanionFacts::gather(&env(&[
            (COMPANION_ENV, OsStr::new("")),
            ("PATH", dir.path().as_os_str()),
        ]));
        assert!(!facts.override_set, "an empty override is not one");
        assert_eq!(
            facts.resolved,
            Resolved::Found(dir.path().join(COMPANION_BIN)),
            "discovery must have run"
        );
    }

    /// The gathering wiring: `$PATH` really is the list discovery walks, and a set override
    /// really does record itself.
    #[test]
    fn gather_walks_the_path_it_was_given_and_records_a_set_override() {
        let dir = dir_with(COMPANION_BIN, 0o755);
        let found = CompanionFacts::gather(&env(&[("PATH", dir.path().as_os_str())]));
        assert_eq!(
            found.resolved,
            Resolved::Found(dir.path().join(COMPANION_BIN))
        );
        assert!(!found.override_set);

        let bin = dir.path().join(COMPANION_BIN);
        let overridden = CompanionFacts::gather(&env(&[(COMPANION_ENV, bin.as_os_str())]));
        assert!(overridden.override_set);
        assert_eq!(overridden.resolved, Resolved::Found(bin));
    }

    /// A `$PATH` that is not UTF-8 is still a search list. Reading it as a `String` would drop it
    /// and report "PATH is unset", a claim the walk never established.
    #[test]
    fn a_non_utf8_path_is_still_searched() {
        use std::os::unix::ffi::OsStrExt;

        let dir = dir_with(COMPANION_BIN, 0o755);
        let mut raw = dir.path().as_os_str().to_os_string();
        raw.push(OsStr::from_bytes(b":/\xff\xfe"));
        let facts = CompanionFacts::gather(&env(&[("PATH", &raw)]));
        assert_eq!(
            facts.resolved,
            Resolved::Found(dir.path().join(COMPANION_BIN))
        );
    }

    #[test]
    fn discovery_prefers_path_over_the_homebrew_prefixes() {
        let on_path = dir_with(COMPANION_BIN, 0o755);
        let brew = dir_with(COMPANION_BIN, 0o755);
        assert_eq!(
            resolve_companion_with(None, path_of(&[on_path.path()]), &[&candidate(&brew)]),
            Resolved::Found(on_path.path().join(COMPANION_BIN))
        );
    }

    /// launchd hands a `.app` a minimal `PATH` that omits Homebrew's bindir; discovery still
    /// finds the `brew install`.
    #[test]
    fn discovery_finds_a_homebrew_prefix_when_the_companion_is_off_path() {
        let empty = tempfile::tempdir().expect("tempdir");
        let brew = dir_with(COMPANION_BIN, 0o755);
        assert_eq!(
            resolve_companion_with(None, path_of(&[empty.path()]), &[&candidate(&brew)]),
            Resolved::Found(brew.path().join(COMPANION_BIN))
        );
    }

    /// The prefixes are ordered (Apple silicon before Intel), so the first match wins.
    #[test]
    fn discovery_takes_the_first_homebrew_prefix_that_matches() {
        let first = dir_with(COMPANION_BIN, 0o755);
        let second = dir_with(COMPANION_BIN, 0o755);
        assert_eq!(
            resolve_companion_with(None, None, &[&candidate(&first), &candidate(&second)]),
            Resolved::Found(first.path().join(COMPANION_BIN))
        );
    }

    #[test]
    fn a_companion_that_is_nowhere_is_absent() {
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve_companion_with(
                None,
                path_of(&[empty.path()]),
                &["/nonexistent/idb_companion"]
            ),
            Resolved::Absent
        );
    }

    /// glass#373: MCP clients routinely spawn glass-mcp with a stripped environment. With no
    /// `$PATH` the standard prefixes are still checked, but an install anywhere else could not
    /// be looked up — which is a different remedy from "install it".
    #[test]
    fn no_path_and_no_companion_in_the_prefixes_says_it_could_not_be_looked_up() {
        assert_eq!(
            resolve_companion_with(None, None, &["/nonexistent/idb_companion"]),
            Resolved::NoSearchPath
        );
    }

    /// Both facts are observed at once: no `$PATH` to search AND an unrunnable copy in a prefix.
    /// The file the user can act on wins — reporting "PATH is unset" would send them to fix an
    /// environment when there is a companion right there to `chmod`.
    #[test]
    fn an_unrunnable_prefix_copy_outranks_a_missing_search_list() {
        let brew = dir_with(COMPANION_BIN, 0o644);
        assert_eq!(
            resolve_companion_with(None, None, &[&candidate(&brew)]),
            Resolved::NotExecutable(brew.path().join(COMPANION_BIN))
        );
    }

    /// A companion found in a prefix answers the question outright, so a missing `$PATH` is not
    /// worth reporting.
    #[test]
    fn no_path_still_resolves_a_companion_in_a_prefix() {
        let brew = dir_with(COMPANION_BIN, 0o755);
        assert_eq!(
            resolve_companion_with(None, None, &[&candidate(&brew)]),
            Resolved::Found(brew.path().join(COMPANION_BIN))
        );
    }

    /// Gathered facts for a resolution that discovery reached (no `GLASS_IDB_COMPANION`).
    fn discovered(resolved: Resolved) -> CompanionFacts {
        CompanionFacts {
            resolved,
            override_set: false,
        }
    }

    /// Gathered facts for a resolution `GLASS_IDB_COMPANION` decided.
    fn overridden(resolved: Resolved) -> CompanionFacts {
        CompanionFacts {
            resolved,
            override_set: true,
        }
    }

    #[test]
    fn the_spawn_runs_the_binary_resolution_found() {
        let bin = PathBuf::from("/opt/homebrew/bin/idb_companion");
        assert_eq!(
            discovered(Resolved::Found(bin.clone())).spawn_target(),
            Ok(bin.as_path())
        );
    }

    /// glass#393: exec'ing it anyway costs a spawn and comes back with a bare `EACCES`, where the
    /// resolution already knows the file and the fix.
    #[test]
    fn a_companion_it_cannot_run_is_refused_by_name_rather_than_spawned() {
        let no = discovered(Resolved::NotExecutable(PathBuf::from(
            "/usr/local/bin/idb_companion",
        )))
        .spawn_target()
        .expect_err("an unrunnable companion must not be spawned");
        assert!(
            no.cause.contains("/usr/local/bin/idb_companion"),
            "the cause must name the file: {}",
            no.cause
        );
        assert!(
            no.remedy.contains("chmod +x"),
            "the remedy must be the permission fix: {}",
            no.remedy
        );
    }

    #[test]
    fn a_companion_that_is_nowhere_is_refused_with_the_install() {
        let no = discovered(Resolved::Absent)
            .spawn_target()
            .expect_err("a missing companion cannot be spawned");
        assert_eq!(no.remedy, INSTALL_REMEDY);
    }

    /// The tap comes first — `idb_companion` is not in Homebrew core, so a bare
    /// `brew install idb-companion` fails. The launch path and the doctor must not differ on it.
    #[test]
    fn the_install_the_launch_path_names_is_the_one_the_doctor_names() {
        assert!(
            INSTALL_REMEDY.contains("brew tap facebook/fb"),
            "the install must tap first: {INSTALL_REMEDY}"
        );
    }

    /// An override skips discovery, so an install the user has not pointed it at is not the fix.
    #[test]
    fn an_override_that_names_nothing_is_refused_by_naming_the_variable() {
        let no = overridden(Resolved::Absent)
            .spawn_target()
            .expect_err("an override naming nothing cannot be spawned");
        assert!(
            no.cause.contains("GLASS_IDB_COMPANION"),
            "must name the variable that decided this: {}",
            no.cause
        );
        assert!(
            !no.remedy.contains("brew install"),
            "installing another copy would not be read while the override stands: {}",
            no.remedy
        );
    }

    #[test]
    fn a_companion_that_could_not_be_looked_up_is_refused_by_naming_path() {
        let no = discovered(Resolved::NoSearchPath)
            .spawn_target()
            .expect_err("nothing was looked up, so nothing can be spawned");
        assert!(
            no.cause.contains("PATH"),
            "must name the unset PATH: {}",
            no.cause
        );
    }

    /// A bare-name override with no `$PATH` is the one way an override reaches `NoSearchPath`.
    /// "Set GLASS_IDB_COMPANION" is what they already did; an absolute path is the fix.
    #[test]
    fn a_bare_name_override_with_no_path_asks_for_an_absolute_path() {
        let no = overridden(Resolved::NoSearchPath)
            .spawn_target()
            .expect_err("a bare name with no PATH resolves to nothing");
        assert!(
            no.cause.contains("GLASS_IDB_COMPANION"),
            "must name the variable it searched for: {}",
            no.cause
        );
        assert!(
            no.remedy.contains("absolute path"),
            "must ask for an absolute path, not for the variable they already set: {}",
            no.remedy
        );
    }

    /// The launch path gets one string, so it has to carry the fix as well as the cause.
    #[test]
    fn the_launch_message_joins_the_cause_to_its_remedy() {
        let no = discovered(Resolved::Absent)
            .spawn_target()
            .expect_err("a missing companion cannot be spawned");
        let msg = no.message();
        assert!(msg.contains(&no.cause) && msg.contains(&no.remedy), "{msg}");
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

        // A short ready deadline keeps the never-served socket from costing the full
        // production timeout.
        const READY: Duration = Duration::from_millis(500);
        // A unique udid keeps this test's socket file name from colliding with another test
        // in the same process (the socket path is keyed on the udid prefix + this pid).
        const UDID: &str = "REAPTEST-0000-0000-0000-000000000000";

        // Retry past a transient ETXTBSY on the freshly-written stub: a sibling test thread's
        // fork can momentarily hold the write fd open, racing our exec. This affects only the
        // just-written fixture, never the installed idb_companion (same rationale as doctor).
        let mut err = None;
        for _ in 0..100 {
            match IdbCompanion::spawn_with(UDID, &stub, READY) {
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
