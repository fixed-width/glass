//! Environment checks for the Linux accessibility backend ("glass doctor"). The
//! pure `a11y_checks` maps gathered facts to `Check`s and is unit-tested without a
//! bus; `checks` gathers the real environment — including a **live probe of the
//! accessibility bus**, because the launcher being installed and the session-bus var
//! being set do NOT mean the a11y bus is actually running (GNOME starts it lazily,
//! only once an AT client enables it). Without that probe, doctor would report a
//! green "ready" while `glass_a11y_*` calls fail at runtime.

use std::time::Duration;

use glass_core::{Check, CheckStatus};

/// Probe whether the AT-SPI accessibility stack is usable.
pub fn checks() -> Vec<Check> {
    let session_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    let registry = find_registry().is_some();
    let bus = probe_a11y_bus();
    a11y_checks(session_bus, registry, bus)
}

/// Pure: build the a11y checks from gathered facts. `bus` is the result of actually
/// trying to reach the accessibility bus (`Ok` = reachable, `Err(reason)` = not).
fn a11y_checks(session_bus: bool, registry: bool, bus: Result<(), String>) -> Vec<Check> {
    let mut checks = Vec::new();

    // The headline: can we actually reach the a11y bus? The launcher being installed
    // and the session-bus var being set do NOT guarantee the bus is running.
    checks.push(match &bus {
        Ok(()) => Check::new(
            "a11y bus",
            CheckStatus::Ok,
            "reachable — glass_a11y_snapshot / glass_a11y_marks / glass_click_element will work",
        ),
        Err(e) => Check::new("a11y bus", CheckStatus::Warn, format!("not reachable: {e}"))
            .with_remedy(
                "the accessibility bus isn't reachable. If it isn't enabled, turn it on: \
                 `gsettings set org.gnome.desktop.interface toolkit-accessibility true`. If it \
                 IS enabled but still unreachable, the bus is likely wedged (the daemon is alive \
                 but its socket file got unlinked from $XDG_RUNTIME_DIR/at-spi) — restart at-spi: \
                 kill the at-spi-bus-launcher / at-spi2-registryd / a11y dbus-daemon processes by \
                 PID, then re-activate with any a11y client (e.g. `dbus-send --session \
                 --dest=org.a11y.Bus --print-reply /org/a11y/bus org.a11y.Bus.GetAddress`). Until \
                 the bus is up, glass_a11y_snapshot / glass_a11y_marks / glass_click_element \
                 return AccessibilityUnavailable; the pixel loop (screenshot / click / type / \
                 diff / wait_stable) is unaffected.",
            ),
    });

    // Preconditions, to help localise a non-reachable bus.
    checks.push(if session_bus {
        Check::new("session bus", CheckStatus::Ok, "DBUS_SESSION_BUS_ADDRESS is set")
    } else {
        Check::new("session bus", CheckStatus::Warn, "DBUS_SESSION_BUS_ADDRESS unset")
            .with_remedy(
                "accessibility needs a session D-Bus; run glass inside a desktop session, or \
                 under `dbus-run-session`",
            )
    });
    checks.push(if registry {
        Check::new("at-spi2 registry", CheckStatus::Ok, "at-spi-bus-launcher found")
    } else {
        Check::new("at-spi2 registry", CheckStatus::Warn, "at-spi-bus-launcher not found")
            .with_remedy("install the AT-SPI registry (e.g. `apt install at-spi2-core`)")
    });
    checks
}

/// The at-spi bus launcher across known libexec/lib locations.
fn find_registry() -> Option<std::path::PathBuf> {
    const CANDIDATES: &[&str] = &[
        "/usr/libexec/at-spi-bus-launcher",
        "/usr/lib/at-spi2-core/at-spi-bus-launcher",
        "/usr/lib/at-spi2/at-spi-bus-launcher",
        "/usr/lib/x86_64-linux-gnu/at-spi2-core/at-spi-bus-launcher",
    ];
    CANDIDATES.iter().map(std::path::PathBuf::from).find(|p| p.is_file())
}

/// Try to reach the accessibility bus exactly the way the reader does — on a private
/// thread + current-thread runtime with a short timeout, so a wedged bus can't hang
/// doctor. `Ok(())` means a connection was established and dropped.
fn probe_a11y_bus() -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let res = (|| -> Result<(), String> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(async {
                atspi::connection::AccessibilityConnection::new()
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        })();
        let _ = tx.send(res);
    });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(r) => r,
        Err(_) => Err("timed out connecting to the accessibility bus".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reachable_bus_is_all_ok() {
        let cs = a11y_checks(true, true, Ok(()));
        assert!(cs.iter().all(|c| c.status == CheckStatus::Ok));
        assert!(cs.iter().any(|c| c.name == "a11y bus" && c.status == CheckStatus::Ok));
    }

    #[test]
    fn unreachable_bus_warns_with_remedy_and_degradation() {
        let cs = a11y_checks(true, true, Err("No such file or directory".into()));
        let bus = cs.iter().find(|c| c.name == "a11y bus").unwrap();
        assert_eq!(bus.status, CheckStatus::Warn);
        let remedy = bus.remedy.as_deref().unwrap();
        // Tells the user how to fix it (both the not-enabled and wedged-bus cases) and
        // what degrades.
        assert!(remedy.contains("toolkit-accessibility"), "remedy: {remedy}");
        assert!(remedy.contains("restart at-spi"), "remedy should cover the wedged bus: {remedy}");
        assert!(remedy.contains("AccessibilityUnavailable"), "remedy: {remedy}");
    }

    #[test]
    fn missing_preconditions_warn_with_remedies() {
        let cs = a11y_checks(false, false, Err("unreachable".into()));
        for name in ["session bus", "at-spi2 registry"] {
            let c = cs.iter().find(|c| c.name == name).unwrap();
            assert_eq!(c.status, CheckStatus::Warn);
            assert!(c.remedy.is_some());
        }
    }
}
