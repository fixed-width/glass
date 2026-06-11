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

/// A private session bus + AT-SPI registry, torn down on drop.
pub struct PrivateBus {
    dbus: Child,
    atspi: Child,
    session_bus_address: String,
    a11y_bus_address: String,
    #[expect(dead_code, reason = "RAII: keep the dbus-daemon stdout pipe open")]
    dbus_stdout: ChildStdout,
}

impl PrivateBus {
    pub fn session_bus_address(&self) -> &str {
        &self.session_bus_address
    }
    pub fn a11y_bus_address(&self) -> &str {
        &self.a11y_bus_address
    }

    pub fn start() -> Result<PrivateBus> {
        let dbus_bin = glass_core::tool_path("GLASS_DBUS_DAEMON", "dbus-daemon");
        let mut dbus = Command::new(&dbus_bin)
            .args(["--session", "--print-address"])
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
        let atspi = match Command::new(&launcher)
            .env("DBUS_SESSION_BUS_ADDRESS", &session_bus_address)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = dbus.kill();
                let _ = dbus.wait();
                return Err(GlassError::Backend(format!("spawn {}: {e}", launcher.display())));
            }
        };

        match resolve_a11y_address(&session_bus_address) {
            Ok(a11y_bus_address) => Ok(PrivateBus {
                dbus,
                atspi,
                session_bus_address,
                a11y_bus_address,
                dbus_stdout,
            }),
            Err(e) => {
                let mut bus = PrivateBus {
                    dbus,
                    atspi,
                    session_bus_address,
                    a11y_bus_address: String::new(),
                    dbus_stdout,
                };
                bus.reap();
                std::mem::forget(bus);
                Err(e)
            }
        }
    }

    fn reap(&mut self) {
        let _ = self.atspi.kill();
        let _ = self.atspi.wait();
        let _ = self.dbus.kill();
        let _ = self.dbus.wait();
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        self.reap();
    }
}

fn read_first_line(stdout: ChildStdout, timeout: Duration) -> Result<(String, ChildStdout)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let n = reader.read_line(&mut line);
        let _ = tx.send(n.map(|_| (line, reader.into_inner())));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok((line, stdout))) => {
            let addr = line.trim().to_string();
            if addr.is_empty() {
                return Err(GlassError::Backend("dbus-daemon printed an empty address".into()));
            }
            Ok((addr, stdout))
        }
        Ok(Err(e)) => Err(GlassError::Backend(format!("read dbus-daemon address: {e}"))),
        Err(_) => Err(GlassError::Backend("timed out reading dbus-daemon address".into())),
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

fn resolve_a11y_address(session_addr: &str) -> Result<String> {
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
        let proxy = A11yBusProxy::new(&conn)
            .await
            .map_err(|e| GlassError::Backend(format!("org.a11y.Bus proxy: {e}")))?;
        let mut last = String::new();
        for _ in 0..50 {
            match proxy.get_address().await {
                Ok(a) if !a.is_empty() => return Ok(a),
                Ok(a) => last = a,
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(GlassError::Backend(format!(
            "org.a11y.Bus.GetAddress did not yield an address (last: {last:?})"
        )))
    })
}
