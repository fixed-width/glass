//! `PrivateBus`: a per-session private D-Bus session bus + AT-SPI registry so a
//! launched app publishes an accessibility tree isolated from the host session.
//! Spawns a minimal-config `dbus-daemon` (session bus, no auto-activatable services) plus
//! `at-spi-bus-launcher`, and resolves the a11y-bus address; reaps both on `Drop` (mirrors
//! `glass-x11`'s `Xvfb`).

#![cfg(target_os = "linux")]

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use glass_core::{GlassError, Result};
use glass_exec_unix::{Resolved, resolve_bin, resolve_path};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard ceiling on the whole a11y-bus resolution (connect → proxy → GetAddress poll). The
/// per-call bounds below don't cover the connect/proxy await, so without this a stalled
/// at-spi bring-up can hang glass_start forever; cap it and fail loud instead.
const A11Y_RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

/// A private session bus + AT-SPI registry, torn down on drop.
pub struct PrivateBus {
    dbus: Child,
    atspi: Child,
    session_bus_address: String,
    a11y_bus_address: String,
    #[expect(dead_code, reason = "RAII: keep the dbus-daemon stdout pipe open")]
    dbus_stdout: ChildStdout,
    // at-spi-bus-launcher writes its socket under $XDG_RUNTIME_DIR/at-spi/; a private
    // dir keeps it off the host's /run/user/UID/at-spi/. Removed on Drop.
    runtime_dir: tempfile::TempDir,
}

impl PrivateBus {
    pub fn session_bus_address(&self) -> &str {
        &self.session_bus_address
    }
    pub fn a11y_bus_address(&self) -> &str {
        &self.a11y_bus_address
    }
    /// The private runtime dir holding this bus's sockets (session-bus + at-spi/). Bind this
    /// into a sandboxed run so the launched app can reach the advertised `unix:path=` sockets.
    pub fn runtime_dir(&self) -> &std::path::Path {
        self.runtime_dir.path()
    }

    pub fn start() -> Result<PrivateBus> {
        let dbus_bin = glass_core::tool_path("GLASS_DBUS_DAEMON", "dbus-daemon");

        // Create the private runtime dir first so the session socket lives inside it.
        let runtime_dir = tempfile::Builder::new()
            .prefix("glass-a11y-")
            .tempdir()
            .map_err(|e| GlassError::Backend(format!("a11y runtime dir: {e}")))?;

        let session_sock = runtime_dir.path().join("session-bus");
        let config_path = runtime_dir.path().join("session-bus.conf");
        // Spawn the private session bus from a minimal generated config rather than
        // `--session`. `--session` loads the system service directories, which makes
        // portals and session managers auto-activatable: a launched GtkApplication probes
        // `org.freedesktop.portal.Desktop` at startup, the bus tries to activate a portal
        // that cannot run in this headless/isolated environment, and GTK blocks on the ~25s
        // D-Bus reply timeout before it maps its window (glass then waits that out locating
        // the window). A config with NO `<servicedir>` makes that probe fail fast
        // (`ServiceUnknown`). a11y is unaffected: `at-spi-bus-launcher`, spawned directly
        // below, owns `org.a11y.Bus` on this bus — it is never D-Bus-activated.
        let config = format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={sock}</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
            sock = xml_escape(&session_sock.display().to_string())
        );
        std::fs::write(&config_path, config)
            .map_err(|e| GlassError::Backend(format!("write a11y bus config: {e}")))?;
        let mut dbus = Command::new(&dbus_bin)
            .arg("--config-file")
            .arg(&config_path)
            .arg("--print-address")
            // No XDG_RUNTIME_DIR pin on the daemon: it declares no activatable services, so
            // nothing inherits its env, and its listen socket is the explicit path above. The
            // launcher we spawn below keeps its OWN XDG_RUNTIME_DIR pinned to the private dir.
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| GlassError::Backend(format!("spawn {dbus_bin}: {e}")))?;
        let stdout = dbus.stdout.take().expect("piped stdout");
        let (session_bus_address, dbus_stdout) = match read_first_line(stdout, READY_TIMEOUT) {
            Ok(v) => v,
            Err(e) => {
                let _ = dbus.kill();
                let _ = dbus.wait();
                return Err(e);
            }
        };

        let launcher = match launcher_or_reason(find_launcher()) {
            Ok(l) => l,
            Err(why) => {
                let _ = dbus.kill();
                let _ = dbus.wait();
                return Err(GlassError::Backend(why));
            }
        };

        let mut atspi = match Command::new(&launcher)
            .env("DBUS_SESSION_BUS_ADDRESS", &session_bus_address)
            .env("XDG_RUNTIME_DIR", runtime_dir.path())
            // at-spi-bus-launcher backs `org.a11y.Status` (IsEnabled/ScreenReaderEnabled) with
            // GSettings, whose default backend persists through the `ca.desrt.dconf` D-Bus
            // service. This bus has no `<servicedir>` (see the config above), so dconf can never
            // be activated — a later `Set` on ScreenReaderEnabled would fail to persist and the
            // property would silently stay at its default `false`, forever hiding
            // accesskit-based apps' a11y trees. The in-memory backend needs no D-Bus service at
            // all, so the setter in `advertise_screen_reader` actually takes.
            .env("GSETTINGS_BACKEND", "memory")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = dbus.kill();
                let _ = dbus.wait();
                return Err(GlassError::Backend(format!(
                    "spawn {}: {e}",
                    launcher.display()
                )));
            }
        };

        match resolve_a11y_address(&session_bus_address) {
            Ok(a11y_bus_address) => Ok(PrivateBus {
                dbus,
                atspi,
                session_bus_address,
                a11y_bus_address,
                dbus_stdout,
                runtime_dir,
            }),
            Err(e) => {
                // With no activation fallback, the directly-spawned launcher is the only path
                // to org.a11y.Bus. If it has already exited, name that specifically instead of
                // returning only the generic resolve timeout.
                let e = match atspi.try_wait() {
                    Ok(Some(status)) => GlassError::Backend(format!(
                        "at-spi-bus-launcher exited ({status}) before claiming org.a11y.Bus: {e}"
                    )),
                    _ => e,
                };
                reap_children(&mut dbus, &mut atspi);
                Err(e)
            }
        }
    }
}

/// Tear down the private bus, atspi-launcher FIRST then the session dbus-daemon.
///
/// SIGTERM-first (graceful), not SIGKILL: `at-spi-bus-launcher` *forks its own*
/// accessibility `dbus-daemon` (--config-file=.../at-spi2/accessibility.conf) as a
/// child, and only tears that grandchild down when it runs its own shutdown. A bare
/// `child.kill()` (SIGKILL) gives the launcher no chance to do that, so its forked
/// dbus-daemon reparents to init and leaks on every session teardown. So signal the
/// launcher with SIGTERM and let it reap its grandchild, then the session bus.
/// Fall back to SIGKILL per child if it doesn't exit within the grace period.
/// Order matters: the launcher must shut its grandchild down before we kill the
/// session bus out from under it.
fn reap_children(dbus: &mut Child, atspi: &mut Child) {
    glass_proc_linux::reap_graceful(atspi, glass_proc_linux::REAP_GRACE);
    glass_proc_linux::reap_graceful(dbus, glass_proc_linux::REAP_GRACE);
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        reap_children(&mut self.dbus, &mut self.atspi);
    }
}

fn read_first_line(stdout: ChildStdout, timeout: Duration) -> Result<(String, ChildStdout)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let n = reader.read_line(&mut line);
        let _ = tx.send(n.map(|count| (count, line, reader.into_inner())));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok((0, _line, _stdout))) => Err(GlassError::Backend(
            "dbus-daemon exited without printing an address (failed to start)".into(),
        )),
        Ok(Ok((_, line, stdout))) => {
            let addr = line.trim().to_string();
            if addr.is_empty() {
                return Err(GlassError::Backend(
                    "dbus-daemon printed an empty address".into(),
                ));
            }
            Ok((addr, stdout))
        }
        Ok(Err(e)) => Err(GlassError::Backend(format!(
            "read dbus-daemon address: {e}"
        ))),
        Err(_) => Err(GlassError::Backend(
            "timed out reading dbus-daemon address".into(),
        )),
    }
}

/// Minimal XML escaping for the socket path spliced into the generated bus config, so an
/// unusual `TMPDIR` (a path containing `&`, `<`, `>`, `"`) can't produce malformed XML that
/// `dbus-daemon` rejects with an opaque "failed to start" error. `&` must be replaced first.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The four paths distributions install `at-spi-bus-launcher` to.
const LAUNCHER_CANDIDATES: &[&str] = &[
    "/usr/libexec/at-spi-bus-launcher",
    "/usr/lib/at-spi2-core/at-spi-bus-launcher",
    "/usr/lib/at-spi2/at-spi-bus-launcher",
    "/usr/lib/x86_64-linux-gnu/at-spi2-core/at-spi-bus-launcher",
];

fn find_launcher() -> Resolved {
    find_launcher_with(
        std::env::var_os("GLASS_ATSPI_LAUNCHER"),
        LAUNCHER_CANDIDATES,
    )
}

/// [`find_launcher`] against explicit inputs — the testable seam (no global env). An override
/// names the launcher outright and is not searched for among `candidates`.
///
/// The scan takes the first runnable candidate and, failing that, reports the first present-but-
/// unrunnable one it walked past rather than [`Resolved::Absent`] — the same rule `resolve_bin`
/// applies to a `$PATH` walk, and for the same reason: that file is what the user can act on.
fn find_launcher_with(override_value: Option<OsString>, candidates: &[&str]) -> Resolved {
    if let Some(p) = override_value.filter(|s| !s.is_empty()) {
        return resolve_path(Path::new(&p));
    }
    let mut first_non_executable = None;
    for cand in candidates.iter().map(Path::new) {
        match resolve_path(cand) {
            Resolved::Found(p) => return Resolved::Found(p),
            Resolved::NotExecutable(p) => {
                first_non_executable.get_or_insert(p);
            }
            Resolved::Absent => {}
        }
    }
    first_non_executable.map_or(Resolved::Absent, Resolved::NotExecutable)
}

/// The launcher to spawn, or why there is none — shared by the preflight and the bring-up so
/// they cannot disagree. A launcher that is present but unrunnable is named as such: telling
/// someone who set `GLASS_ATSPI_LAUNCHER` themselves to install at-spi2-core sends them to fix
/// something that is not broken.
fn launcher_or_reason(resolved: Resolved) -> std::result::Result<PathBuf, String> {
    match resolved {
        Resolved::Found(p) => Ok(p),
        Resolved::NotExecutable(p) => Err(format!(
            "at-spi-bus-launcher at {} is not executable",
            p.display()
        )),
        Resolved::Absent => Err(
            "at-spi-bus-launcher not found (install at-spi2-core), or set GLASS_ATSPI_LAUNCHER"
                .into(),
        ),
    }
}

/// Cheap, non-spawning preflight: are the binaries a private a11y bus needs resolvable on
/// this host? Lets a caller decide whether a best-effort (default-on) a11y launch is worth
/// attempting or should quietly fall back to pixel-only, without the cost and teardown churn
/// of actually starting the bus. Not a guarantee [`PrivateBus::start`] will succeed — a
/// resolvable binary can still fail to spawn — but it catches the common "AT-SPI not
/// installed" and misconfigured-path cases.
pub fn available() -> std::result::Result<(), String> {
    available_with(
        find_launcher(),
        &glass_core::tool_path("GLASS_DBUS_DAEMON", "dbus-daemon"),
        std::env::var_os("PATH").as_deref(),
    )
}

/// [`available`] against explicit inputs — the testable seam (no global env), so a test can force
/// any branch without `set_var` racing a concurrent env reader. `path` is the search list a bare
/// `dbus-daemon` is looked up in; the bring-up spawns from this same process, so it is the list
/// the spawn will use.
fn available_with(
    launcher: Resolved,
    dbus: &str,
    path: Option<&OsStr>,
) -> std::result::Result<(), String> {
    launcher_or_reason(launcher)?;
    match resolve_bin(dbus, path) {
        Resolved::Found(_) => Ok(()),
        Resolved::NotExecutable(p) => {
            Err(format!("dbus-daemon at {} is not executable", p.display()))
        }
        Resolved::Absent => Err(format!(
            "dbus-daemon ({dbus}) not found (install dbus, or set GLASS_DBUS_DAEMON to its path)"
        )),
    }
}

#[zbus::proxy(
    interface = "org.a11y.Bus",
    default_service = "org.a11y.Bus",
    default_path = "/org/a11y/bus"
)]
trait A11yBus {
    fn get_address(&self) -> zbus::Result<String>;
}

/// `org.a11y.Status` — the interface accesskit watches for `ScreenReaderEnabled` — lives on
/// the same `org.a11y.Bus` service, at `/org/a11y/bus`.
const A11Y_BUS_SERVICE: &str = "org.a11y.Bus";
const A11Y_BUS_PATH: &str = "/org/a11y/bus";
const A11Y_STATUS_IFACE: &str = "org.a11y.Status";

/// Advertise a screen reader on the private a11y bus (setting the `org.a11y.Status`
/// `ScreenReaderEnabled` and `IsEnabled` properties true) so accesskit-based toolkits
/// (egui/winit) activate their AT-SPI adapter and publish a tree; GTK/Qt register regardless.
/// Best-effort: a read-only or absent Status interface just leaves such apps dormant (the
/// prior behavior), so a failure here must not abort the launch. (`org.a11y.Status`'s typed
/// proxy exposes getters only, so set the properties via the low-level `Proxy`.)
async fn advertise_screen_reader(conn: &zbus::Connection) {
    let status =
        match zbus::Proxy::new(conn, A11Y_BUS_SERVICE, A11Y_BUS_PATH, A11Y_STATUS_IFACE).await {
            Ok(status) => status,
            Err(e) => {
                eprintln!(
                    "glass: org.a11y.Status proxy failed, cannot advertise screen reader \
                 (AccessKit-based apps like egui may not publish an a11y tree): {e}"
                );
                return;
            }
        };
    if let Err(e) = status.set_property("ScreenReaderEnabled", true).await {
        eprintln!("glass: set ScreenReaderEnabled failed: {e}");
    }
    if let Err(e) = status.set_property("IsEnabled", true).await {
        eprintln!("glass: set IsEnabled failed: {e}");
    }
    match status.get_property::<bool>("ScreenReaderEnabled").await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "glass: ScreenReaderEnabled did not persist after being set \
                 (AccessKit-based apps like egui may not publish an a11y tree)"
            );
        }
        Err(e) => {
            eprintln!(
                "glass: readback of ScreenReaderEnabled failed, cannot confirm the advert \
                 stuck (AccessKit-based apps like egui may not publish an a11y tree): {e}"
            );
        }
    }
}

fn resolve_a11y_address(session_addr: &str) -> Result<String> {
    // Run on a dedicated OS thread. `PrivateBus::start` is synchronous but is reached (via
    // `start_app`) from inside the MCP's multi-thread runtime. Building a runtime there
    // panics ("Cannot start a runtime from within a runtime"), and a `current_thread`
    // runtime nested in another runtime has a timer that never advances — so the bounds
    // below would never fire. A fresh thread has no ambient runtime, so `block_on` and its
    // timer work in every caller context (sync test, async worker, or spawn_blocking task).
    let session_addr = session_addr.to_string();
    std::thread::spawn(move || -> Result<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| GlassError::Backend(format!("runtime: {e}")))?;
        rt.block_on(async move {
            let resolve = async {
                let addr: zbus::Address = session_addr
                    .as_str()
                    .try_into()
                    .map_err(|e| GlassError::Backend(format!("bad session address: {e}")))?;
                let conn = zbus::connection::Builder::address(addr)
                    .map_err(|e| GlassError::Backend(format!("session conn builder: {e}")))?
                    .build()
                    .await
                    .map_err(|e| GlassError::Backend(format!("connect session bus: {e}")))?;
                let proxy = A11yBusProxy::new(&conn)
                    .await
                    .map_err(|e| GlassError::Backend(format!("org.a11y.Bus proxy: {e}")))?;
                let mut last = String::new();
                let mut last_err: Option<String> = None;
                for _ in 0..50 {
                    match proxy.get_address().await {
                        Ok(a) if !a.is_empty() => {
                            advertise_screen_reader(&conn).await;
                            return Ok(a);
                        }
                        Ok(a) => last = a,
                        Err(e) => last_err = Some(e.to_string()),
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(GlassError::Backend(format!(
                    "org.a11y.Bus.GetAddress did not yield an address after 5s (last ok: {last:?}, last err: {last_err:?})"
                )))
            };
            match tokio::time::timeout(A11Y_RESOLVE_TIMEOUT, resolve).await {
                Ok(result) => result,
                Err(_) => Err(GlassError::Backend(format!(
                    "a11y bus bring-up did not complete within {A11Y_RESOLVE_TIMEOUT:?} (at-spi-bus-launcher unresponsive)"
                ))),
            }
        })
    })
    .join()
    .map_err(|_| GlassError::Backend("a11y resolver thread panicked".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    /// A directory holding `name` at `mode`. Drives the seams with real files rather than setting
    /// `GLASS_ATSPI_LAUNCHER`/`PATH` — mutating the process environment races every other test in
    /// this binary (and is `unsafe` from edition 2024 on).
    fn dir_with(name: &str, mode: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join(name);
        std::fs::write(&bin, b"").expect("write");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).expect("chmod");
        dir
    }

    /// glass#374: the override was accepted on `is_file()`. Reporting a mere `None` here was the
    /// second half of the defect — the caller then said "install at-spi2-core" about a path the
    /// user had named themselves.
    #[test]
    fn a_non_executable_launcher_override_is_reported_as_present_but_unrunnable() {
        let dir = dir_with("at-spi-bus-launcher", 0o644);
        let launcher = dir.path().join("at-spi-bus-launcher");
        assert_eq!(
            find_launcher_with(Some(launcher.clone().into_os_string()), &[]),
            Resolved::NotExecutable(launcher)
        );
    }

    #[test]
    fn an_executable_launcher_override_is_accepted() {
        let dir = dir_with("at-spi-bus-launcher", 0o755);
        let launcher = dir.path().join("at-spi-bus-launcher");
        assert_eq!(
            find_launcher_with(Some(launcher.clone().into_os_string()), &[]),
            Resolved::Found(launcher)
        );
    }

    /// A non-executable candidate does not stop the scan: a later one that IS runnable wins.
    #[test]
    fn the_candidate_scan_walks_past_a_non_executable_one() {
        let broken = dir_with("at-spi-bus-launcher", 0o644);
        let working = dir_with("at-spi-bus-launcher", 0o755);
        let candidates = [
            broken
                .path()
                .join("at-spi-bus-launcher")
                .to_str()
                .expect("utf-8 temp path")
                .to_owned(),
            working
                .path()
                .join("at-spi-bus-launcher")
                .to_str()
                .expect("utf-8 temp path")
                .to_owned(),
        ];
        let candidates: Vec<&str> = candidates.iter().map(String::as_str).collect();
        assert_eq!(
            find_launcher_with(None, &candidates),
            Resolved::Found(working.path().join("at-spi-bus-launcher"))
        );
    }

    /// Nothing runnable in the whole scan still beats `Absent`: an at-spi2-core install whose
    /// launcher lost its execute bit is a chmod away, not a reinstall.
    #[test]
    fn a_scan_finding_only_a_non_executable_candidate_names_it() {
        let dir = dir_with("at-spi-bus-launcher", 0o644);
        let launcher = dir.path().join("at-spi-bus-launcher");
        let candidates = [launcher.to_str().expect("utf-8 temp path")];
        assert_eq!(
            find_launcher_with(None, &candidates),
            Resolved::NotExecutable(launcher)
        );
    }

    /// Both halves resolvable is the only Ok.
    #[test]
    fn available_is_ok_when_both_binaries_are_runnable() {
        let launcher = dir_with("at-spi-bus-launcher", 0o755);
        let daemon = dir_with("dbus-daemon", 0o755);
        available_with(
            Resolved::Found(launcher.path().join("at-spi-bus-launcher")),
            daemon
                .path()
                .join("dbus-daemon")
                .to_str()
                .expect("utf-8 temp path"),
            None,
        )
        .expect("two runnable binaries are available");
    }

    /// The preflight's other half: an explicit `$GLASS_DBUS_DAEMON` that exists but cannot be run.
    #[test]
    fn available_errors_when_an_explicit_dbus_daemon_path_is_not_executable() {
        let dir = dir_with("dbus-daemon", 0o644);
        let err = available_with(
            Resolved::Found(PathBuf::from("/bin/true")),
            dir.path()
                .join("dbus-daemon")
                .to_str()
                .expect("utf-8 temp path"),
            None,
        )
        .expect_err("a dbus-daemon that cannot be run must be reported");
        assert!(err.contains("not executable"), "actionable: {err}");
    }

    /// A bare `dbus-daemon` is looked up on the search list, so one that is simply not installed
    /// is caught by the preflight instead of surfacing as a spawn failure mid-bring-up.
    #[test]
    fn available_errors_when_a_bare_dbus_daemon_is_not_on_the_search_list() {
        let empty = tempfile::tempdir().expect("tempdir");
        let path = std::env::join_paths([empty.path()]).expect("join paths");
        let err = available_with(
            Resolved::Found(PathBuf::from("/bin/true")),
            "dbus-daemon",
            Some(&path),
        )
        .expect_err("a bare name absent from the search list must be reported");
        assert!(err.contains("not found"), "actionable: {err}");
    }

    #[test]
    fn available_finds_a_bare_dbus_daemon_on_the_search_list() {
        let dir = dir_with("dbus-daemon", 0o755);
        let path = std::env::join_paths([dir.path()]).expect("join paths");
        available_with(
            Resolved::Found(PathBuf::from("/bin/true")),
            "dbus-daemon",
            Some(&path),
        )
        .expect("a bare name on the search list resolves");
    }

    /// An unresolvable launcher must report unavailable with an actionable message, whether or
    /// not at-spi2-core is installed on the test host.
    #[test]
    fn available_errors_when_the_atspi_launcher_is_missing() {
        let err = available_with(Resolved::Absent, "/bin/true", None)
            .expect_err("missing launcher must report unavailable");
        assert!(
            err.contains("at-spi-bus-launcher") && err.contains("install at-spi2-core"),
            "actionable message: {err}"
        );
    }

    /// The regression this branch would otherwise have introduced: a launcher the user named
    /// themselves, present but unrunnable, must be named — not reported as an absent package.
    #[test]
    fn available_names_a_launcher_that_is_present_but_not_executable() {
        let dir = dir_with("at-spi-bus-launcher", 0o644);
        let launcher = dir.path().join("at-spi-bus-launcher");
        let err = available_with(Resolved::NotExecutable(launcher.clone()), "/bin/true", None)
            .expect_err("a launcher that cannot be run must report unavailable");
        assert!(
            err.contains(&launcher.display().to_string()) && err.contains("not executable"),
            "must name the file and say why: {err}"
        );
        assert!(
            !err.contains("install at-spi2-core"),
            "the package is installed; sending the user to reinstall it is the misdirection: {err}"
        );
    }

    /// The other preflight branch: an explicit `$GLASS_DBUS_DAEMON` path that is not a file.
    #[test]
    fn available_errors_when_an_explicit_dbus_daemon_path_is_missing() {
        let err = available_with(
            Resolved::Found(PathBuf::from("/bin/true")),
            "/nonexistent/dbus-daemon",
            None,
        )
        .expect_err("an explicit dbus-daemon path that is not a file must be reported");
        assert!(
            err.contains("not found") && err.contains("/nonexistent/dbus-daemon"),
            "actionable: {err}"
        );
    }

    #[test]
    #[ignore = "spawns dbus-daemon + at-spi-bus-launcher; run explicitly"]
    fn session_bus_is_a_path_socket_in_the_private_dir() {
        let bus = PrivateBus::start().expect("private bus");
        let addr = bus.session_bus_address();
        assert!(
            addr.starts_with("unix:path="),
            "session bus must be a path socket, got {addr}"
        );
        let dir = bus.runtime_dir().to_string_lossy().into_owned();
        assert!(
            addr.contains(&dir),
            "socket {addr} must live in the private runtime dir {dir}"
        );
    }

    #[test]
    #[ignore = "spawns two private buses; run explicitly"]
    fn two_instances_have_distinct_socket_paths() {
        let a = PrivateBus::start().expect("bus a");
        let b = PrivateBus::start().expect("bus b");
        assert_ne!(a.session_bus_address(), b.session_bus_address());
        assert_ne!(a.runtime_dir(), b.runtime_dir());
    }

    /// The private session bus must expose NO auto-activatable services. glass spawns
    /// `at-spi-bus-launcher` directly (it owns `org.a11y.Bus`), and the generated bus config
    /// declares no `<servicedir>` — so nothing can be D-Bus-activated. That is what stops a
    /// launched GtkApplication's startup portal probe from triggering an activation that blocks
    /// the launch for the ~25s D-Bus reply timeout. Asserting on `ListActivatableNames` proves
    /// the property directly from the (zero) servicedir count, independent of whether any
    /// particular service (e.g. a portal) happens to be installed on the test host.
    #[test]
    #[ignore = "spawns dbus-daemon + at-spi-bus-launcher; run via test-a11y.sh"]
    fn private_bus_has_no_auto_activatable_services() {
        let bus = PrivateBus::start().expect("private bus");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let activatable = rt.block_on(async {
            let addr: zbus::Address = bus
                .session_bus_address()
                .try_into()
                .expect("parse session address");
            let conn = zbus::connection::Builder::address(addr)
                .expect("address builder")
                .build()
                .await
                .expect("connect session bus");
            let dbus_proxy = zbus::fdo::DBusProxy::new(&conn).await.expect("DBus proxy");
            dbus_proxy
                .list_activatable_names()
                .await
                .expect("list activatable names")
        });
        // The bus is always activatable as itself; nothing else may be. Anything extra means a
        // <servicedir> leaked in (e.g. a portal) — exactly what caused the ~25s startup hang.
        let extra: Vec<_> = activatable
            .iter()
            .filter(|n| n.as_str() != "org.freedesktop.DBus")
            .collect();
        assert!(
            extra.is_empty(),
            "private bus must expose no auto-activatable services, found: {extra:?}"
        );
    }

    /// With no service dirs, `org.a11y.Bus` is owned *only* by the directly-spawned launcher —
    /// there is no activation fallback. If that launcher never claims the name (here a stand-in
    /// that exits immediately without claiming it), `PrivateBus::start` must fail loudly and
    /// promptly — naming the dead launcher — rather than hanging or returning a broken address.
    ///
    /// Drives `PrivateBus::start`, which resolves the launcher from the environment, so this one
    /// must set `GLASS_ATSPI_LAUNCHER` on the process — hence the `unsafe_code` opt-out.
    #[test]
    #[ignore = "spawns a private dbus-daemon; run via test-a11y.sh"]
    #[allow(unsafe_code)]
    fn launcher_that_never_claims_the_name_fails_fast() {
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: test-a11y.sh runs this suite with `--test-threads=1`, so no other
                // thread is reading the environment while it is mutated.
                unsafe { std::env::remove_var("GLASS_ATSPI_LAUNCHER") };
            }
        }
        // /bin/true is a real file (find_launcher accepts it) that exits 0 without ever
        // claiming org.a11y.Bus.
        // SAFETY: as above — single-threaded harness (`--test-threads=1`).
        unsafe { std::env::set_var("GLASS_ATSPI_LAUNCHER", "/bin/true") };
        let _guard = EnvGuard;

        let started = Instant::now();
        let res = PrivateBus::start();
        let elapsed = started.elapsed();
        // `PrivateBus` isn't `Debug`, so match rather than `expect_err`.
        let err = match res {
            Ok(_) => panic!("a launcher that never claims org.a11y.Bus must fail the bring-up"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("at-spi-bus-launcher exited"),
            "the error should name the dead launcher, got: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "bring-up must fail promptly (bounded by the resolve poll), took {elapsed:?}"
        );
    }

    /// accesskit-based toolkits (egui/winit) register + publish their AT-SPI tree only when
    /// `org.a11y.Status.ScreenReaderEnabled` is true — the signal a screen reader sends. glass's
    /// private bus must advertise it, or such apps stay dormant and invisible to the reader
    /// (while GTK, which doesn't gate on it, works). Read the flag back off the real
    /// at-spi-bus-launcher to prove the setter took, not just local state.
    #[test]
    #[ignore = "spawns dbus-daemon + at-spi-bus-launcher; run under a throwaway XDG_RUNTIME_DIR (see test-a11y.sh)"]
    fn private_bus_advertises_a_screen_reader_for_accesskit_apps() {
        let bus = PrivateBus::start().expect("private bus");
        let enabled = read_screen_reader_enabled(bus.session_bus_address())
            .expect("read org.a11y.Status.ScreenReaderEnabled");
        assert!(
            enabled,
            "glass's private a11y bus must advertise ScreenReaderEnabled=true"
        );
    }

    fn read_screen_reader_enabled(session_addr: &str) -> Result<bool> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| GlassError::Backend(format!("runtime: {e}")))?;
        rt.block_on(async {
            let addr: zbus::Address = session_addr
                .try_into()
                .map_err(|e| GlassError::Backend(format!("bad session address: {e}")))?;
            let conn = zbus::connection::Builder::address(addr)
                .map_err(|e| GlassError::Backend(format!("session conn builder: {e}")))?
                .build()
                .await
                .map_err(|e| GlassError::Backend(format!("connect session bus: {e}")))?;
            let status =
                zbus::Proxy::new(&conn, A11Y_BUS_SERVICE, A11Y_BUS_PATH, A11Y_STATUS_IFACE)
                    .await
                    .map_err(|e| GlassError::Backend(format!("org.a11y.Status proxy: {e}")))?;
            status
                .get_property::<bool>("ScreenReaderEnabled")
                .await
                .map_err(|e| GlassError::Backend(format!("get ScreenReaderEnabled: {e}")))
        })
    }
}
