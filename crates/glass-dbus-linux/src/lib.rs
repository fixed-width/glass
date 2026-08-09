//! `PrivateBus`: a per-session private D-Bus session bus + AT-SPI registry so a
//! launched app publishes an accessibility tree isolated from the host session.
//! Spawns a minimal-config `dbus-daemon` (session bus, no auto-activatable services) plus
//! `at-spi-bus-launcher`, and resolves the a11y-bus address; reaps both on `Drop` (mirrors
//! `glass-x11`'s `Xvfb`).

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use glass_core::{GlassError, Result};
use glass_exec_unix::{Resolved, is_executable_file, resolve_bin};

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

        let launcher = match find_launcher() {
            Some(l) => l,
            None => {
                let _ = dbus.kill();
                let _ = dbus.wait();
                return Err(GlassError::Backend(
                    "at-spi-bus-launcher not found (install at-spi2-core), or set GLASS_ATSPI_LAUNCHER".into(),
                ));
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

fn find_launcher() -> Option<PathBuf> {
    find_launcher_with(
        std::env::var_os("GLASS_ATSPI_LAUNCHER"),
        LAUNCHER_CANDIDATES,
    )
}

/// [`find_launcher`] against explicit inputs — the testable seam (no global env). An override
/// names the launcher outright and is not searched for among `candidates`; either way the result
/// must be runnable, since the only thing done with it is spawning it.
fn find_launcher_with(override_value: Option<OsString>, candidates: &[&str]) -> Option<PathBuf> {
    if let Some(p) = override_value.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        return is_executable_file(&p).then_some(p);
    }
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| is_executable_file(p))
}

/// Cheap, non-spawning preflight: are the binaries a private a11y bus needs resolvable on
/// this host? Lets a caller decide whether a best-effort (default-on) a11y launch is worth
/// attempting or should quietly fall back to pixel-only, without the cost and teardown churn
/// of actually starting the bus. Not a guarantee [`PrivateBus::start`] will succeed — a
/// resolvable binary can still fail to spawn — but it catches the common "AT-SPI not
/// installed" and misconfigured-path cases.
pub fn available() -> std::result::Result<(), String> {
    // `tool_path` returns an explicit path (from GLASS_DBUS_DAEMON) or the bare "dbus-daemon"
    // name resolved on PATH at spawn time. Only an explicit path can be checked here.
    available_with(
        find_launcher(),
        &glass_core::tool_path("GLASS_DBUS_DAEMON", "dbus-daemon"),
    )
}

/// [`available`] against already-resolved inputs — the testable seam (no global env), so a test
/// can force either branch without `set_var` racing a concurrent env reader.
fn available_with(launcher: Option<PathBuf>, dbus: &str) -> std::result::Result<(), String> {
    if launcher.is_none() {
        return Err(
            "at-spi-bus-launcher not found (install at-spi2-core), or set GLASS_ATSPI_LAUNCHER"
                .into(),
        );
    }
    if dbus.contains('/') {
        match resolve_bin(dbus, None) {
            Resolved::Found(_) => {}
            Resolved::NotExecutable(p) => {
                return Err(format!("dbus-daemon at {} is not executable", p.display()));
            }
            Resolved::Absent => return Err(format!("dbus-daemon not found at {dbus}")),
        }
    }
    Ok(())
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

    /// glass#374: the launcher was accepted on `is_file()`, so a mode-644 override read as present
    /// and `PrivateBus::start` then failed at spawn. Drives the seam directly rather than setting
    /// `GLASS_ATSPI_LAUNCHER` — mutating the process environment races every other test in this
    /// binary (and is `unsafe` from edition 2024 on).
    #[test]
    fn a_non_executable_launcher_override_is_not_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let launcher = dir.path().join("at-spi-bus-launcher");
        std::fs::write(&launcher, b"").expect("write");
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            find_launcher_with(Some(launcher.into_os_string()), &[]),
            None
        );
    }

    #[test]
    fn an_executable_launcher_override_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let launcher = dir.path().join("at-spi-bus-launcher");
        std::fs::write(&launcher, b"").expect("write");
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(
            find_launcher_with(Some(launcher.clone().into_os_string()), &[]),
            Some(launcher)
        );
    }

    /// A non-executable candidate does not stop the scan: a later one that IS runnable wins.
    #[test]
    fn the_candidate_scan_walks_past_a_non_executable_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broken = dir.path().join("broken");
        let working = dir.path().join("working");
        std::fs::write(&broken, b"").expect("write");
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        std::fs::write(&working, b"").expect("write");
        std::fs::set_permissions(&working, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let candidates = [
            broken.to_str().expect("utf-8 temp path"),
            working.to_str().expect("utf-8 temp path"),
        ];
        assert_eq!(find_launcher_with(None, &candidates), Some(working));
    }

    /// The preflight's other half: an explicit `$GLASS_DBUS_DAEMON` that exists but cannot be run.
    #[test]
    fn available_errors_when_an_explicit_dbus_daemon_path_is_not_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let daemon = dir.path().join("dbus-daemon");
        std::fs::write(&daemon, b"").expect("write");
        std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = available_with(
            Some(PathBuf::from("/bin/true")),
            daemon.to_str().expect("utf-8 temp path"),
        )
        .expect_err("a dbus-daemon that cannot be run must be reported");
        assert!(err.contains("not executable"), "actionable: {err}");
    }

    /// An unresolvable launcher must report unavailable with an actionable message, whether or
    /// not at-spi2-core is installed on the test host. Drives the seam directly rather than
    /// pointing `GLASS_ATSPI_LAUNCHER` at a nonexistent path — mutating the process environment
    /// would race every other test in this binary (and is `unsafe` from edition 2024 on).
    #[test]
    fn available_errors_when_the_atspi_launcher_is_missing() {
        let err = available_with(None, "dbus-daemon")
            .expect_err("missing launcher must report unavailable");
        assert!(
            err.contains("at-spi-bus-launcher"),
            "actionable message: {err}"
        );
    }

    /// The other preflight branch: an explicit `$GLASS_DBUS_DAEMON` path that is not a file.
    #[test]
    fn available_errors_when_an_explicit_dbus_daemon_path_is_missing() {
        let err = available_with(Some(PathBuf::from("/bin/true")), "/nonexistent/dbus-daemon")
            .expect_err("an explicit dbus-daemon path that is not a file must be reported");
        assert!(err.contains("dbus-daemon not found"), "actionable: {err}");
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
