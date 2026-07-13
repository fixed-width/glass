//! `PrivateBus`: a per-session private D-Bus session bus + AT-SPI registry so a
//! launched app publishes an accessibility tree isolated from the host session.
//! Spawns `dbus-daemon --session --print-address` and `at-spi-bus-launcher`, and
//! resolves the a11y-bus address; reaps both on `Drop` (mirrors `glass-x11`'s `Xvfb`).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use glass_core::{GlassError, Result};

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
        let mut dbus = Command::new(&dbus_bin)
            .args([
                "--session",
                &format!("--address=unix:path={}", session_sock.display()),
                "--print-address",
            ])
            // Pin XDG_RUNTIME_DIR to the private dir: when this bus *D-Bus-activates*
            // org.a11y.Bus (which it does whenever the launcher we spawn directly doesn't
            // claim the name first), the activated at-spi-bus-launcher inherits this env.
            // Without it, activation falls back to the ambient XDG_RUNTIME_DIR and attaches
            // to the *host* accessibility bus — breaking isolation and risking a wedge on
            // the contended host bus. Keep activation inside the private dir.
            .env("XDG_RUNTIME_DIR", runtime_dir.path())
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

fn find_launcher() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GLASS_ATSPI_LAUNCHER").filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    const CANDIDATES: &[&str] = &[
        "/usr/libexec/at-spi-bus-launcher",
        "/usr/lib/at-spi2-core/at-spi-bus-launcher",
        "/usr/lib/at-spi2/at-spi-bus-launcher",
        "/usr/lib/x86_64-linux-gnu/at-spi2-core/at-spi-bus-launcher",
    ];
    CANDIDATES.iter().map(PathBuf::from).find(|p| p.is_file())
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
    let Ok(status) =
        zbus::Proxy::new(conn, A11Y_BUS_SERVICE, A11Y_BUS_PATH, A11Y_STATUS_IFACE).await
    else {
        return;
    };
    let _ = status.set_property("ScreenReaderEnabled", true).await;
    let _ = status.set_property("IsEnabled", true).await;
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

    /// Regression: the private a11y bus must stay inside our private runtime dir even
    /// when `org.a11y.Bus` is brought up by **D-Bus activation** rather than the launcher
    /// we spawn directly. We force that path by pointing `GLASS_ATSPI_LAUNCHER` at a no-op
    /// (`/bin/true` exits without claiming the name — exactly the zombie observed in
    /// practice), so the session bus must activate the real launcher itself. If the
    /// activated launcher escapes to the ambient `XDG_RUNTIME_DIR`, it attaches to the
    /// host accessibility bus — breaking isolation and (as observed) wedging on the
    /// contended host bus. Run this under a throwaway `XDG_RUNTIME_DIR` (test-a11y.sh does)
    /// so "ambient" is a sacrificial dir, never the operator's real `/run/user/UID`.
    #[test]
    #[ignore = "spawns a private dbus-daemon + activates at-spi; run via test-a11y.sh"]
    fn activation_fallback_stays_in_the_private_runtime_dir() {
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                std::env::remove_var("GLASS_ATSPI_LAUNCHER");
            }
        }
        // /bin/true is a real file (find_launcher accepts it) that exits 0 immediately,
        // so it never claims org.a11y.Bus → the session bus must activate the real one.
        std::env::set_var("GLASS_ATSPI_LAUNCHER", "/bin/true");
        let _guard = EnvGuard;

        let bus = PrivateBus::start().expect("a11y bring-up via D-Bus activation");
        let private_dir = bus.runtime_dir().to_string_lossy().into_owned();
        let a11y = bus.a11y_bus_address();
        assert!(
            a11y.contains(&private_dir),
            "a11y bus {a11y} escaped the private runtime dir {private_dir} \
             (activation inherited the ambient XDG_RUNTIME_DIR — isolation breach)"
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
