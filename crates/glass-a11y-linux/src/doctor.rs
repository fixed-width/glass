//! Environment checks for the Linux accessibility backend ("glass doctor"). The
//! pure `a11y_checks` maps gathered facts to `Check`s and is unit-tested without a
//! bus; `checks` gathers the real environment — including a **live probe of the
//! accessibility bus**, because the launcher being installed and the session-bus var
//! being set do NOT mean the a11y bus is actually running (GNOME starts it lazily,
//! only once an AT client enables it). Without that probe, doctor would report a
//! green "ready" while `glass_a11y_*` calls fail at runtime.

use std::time::Duration;

use glass_core::capability::CapabilityStatus;
use glass_core::{Check, CheckStatus};

/// Live: is the AT-SPI bus launcher installed, so glass can spawn its private a11y bus?
/// This is the desktop-a11y capability signal for the Linux backends — the *same* fact the
/// doctor's head "a11y" check reads (both go through [`find_registry`]), so `glass_capabilities`
/// and `glass doctor` can't drift. It is only a precondition ("glass can do a11y at all"), never
/// a promise that a given window exposes a tree — that's up to the app.
pub fn accessibility_launcher_present() -> bool {
    find_registry().is_some()
}

/// The desktop-`accessibility` capability cell for a Linux backend, from the launcher-present
/// signal. Shared by glass-x11 and glass-wayland (identical stacks) so their note can't drift.
pub const fn accessibility_capability(launcher_present: bool) -> CapabilityStatus {
    if launcher_present {
        CapabilityStatus::supported()
    } else {
        CapabilityStatus::requires_setup(
            "AT-SPI not installed; install at-spi2-core so glass can spawn its private a11y bus",
        )
    }
}

/// Read `/proc` for dbus-daemon entries (impure). Errors degrade to an empty list.
fn read_proc_entries() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in dir.flatten() {
        let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if comm != "dbus-daemon" {
            continue;
        }
        let cmdline = String::from_utf8_lossy(
            &std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default(),
        )
        .replace('\0', " ");
        let ppid = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| parse_ppid_from_stat(&s))
            .unwrap_or(0);
        out.push(ProcEntry {
            comm,
            cmdline,
            ppid,
        });
    }
    out
}

/// Ask the host session bus what a11y address it advertises (`org.a11y.Bus.GetAddress`).
/// Private thread + current-thread runtime + short timeout, like `probe_a11y_bus`.
fn advertised_a11y_address() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let res = (|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            rt.block_on(async {
                let conn = zbus::Connection::session().await.ok()?;
                let proxy =
                    zbus::Proxy::new(&conn, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
                        .await
                        .ok()?;
                proxy
                    .call_method("GetAddress", &())
                    .await
                    .ok()?
                    .body()
                    .deserialize::<String>()
                    .ok()
            })
        })();
        let _ = tx.send(res);
    });
    rx.recv_timeout(Duration::from_secs(3)).ok().flatten()
}

/// Gather host a11y facts (impure: live probe + GetAddress + `/proc`). Read-only — never mutates.
fn gather_host_a11y() -> HostA11yFacts {
    let session_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    let probe_ok = probe_a11y_bus().is_ok();
    let advertised = advertised_a11y_address();
    let socket_present = advertised
        .as_deref()
        .and_then(socket_path_from_address)
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);
    let bus = classify_host_bus(probe_ok, advertised.as_deref(), socket_present);
    let orphaned_daemons = count_orphaned_a11y_daemons(&read_proc_entries());
    HostA11yFacts {
        session_bus,
        bus,
        orphaned_daemons,
    }
}

/// Probe whether the AT-SPI accessibility stack is usable.
pub fn checks() -> Vec<Check> {
    a11y_checks(accessibility_launcher_present(), &gather_host_a11y())
}

/// Health of the *host* (operator's desktop) AT-SPI bus — distinct from glass's private bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostBusState {
    Reachable,
    Wedged { address: String },
    NotRunning,
}

/// Facts about the host a11y environment, gathered once and mapped to checks.
pub(crate) struct HostA11yFacts {
    pub session_bus: bool,
    pub bus: HostBusState,
    /// a11y dbus-daemons reparented to init (PPID 1) — leaked by a dead launcher.
    pub orphaned_daemons: usize,
}

/// One `/proc/<pid>` entry, gathered impurely, classified purely.
pub(crate) struct ProcEntry {
    pub comm: String,
    pub cmdline: String,
    pub ppid: u32,
}

/// Extract the filesystem socket path from a D-Bus `unix:path=…[,guid=…]` address.
fn socket_path_from_address(addr: &str) -> Option<&str> {
    addr.split(',').find_map(|kv| kv.strip_prefix("unix:path="))
}

/// Classify the host bus from the probe result + what (if anything) it advertises.
fn classify_host_bus(
    probe_ok: bool,
    advertised: Option<&str>,
    socket_present: bool,
) -> HostBusState {
    if probe_ok {
        return HostBusState::Reachable;
    }
    match advertised {
        Some(addr) if !socket_present => HostBusState::Wedged {
            address: addr.to_string(),
        },
        _ => HostBusState::NotRunning,
    }
}

/// Parse the parent PID from `/proc/<pid>/stat`. `comm` is parenthesized and may contain
/// spaces/parens, so split after the LAST ')': fields are then `state ppid …`.
fn parse_ppid_from_stat(stat: &str) -> Option<u32> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Count a11y dbus-daemons reparented to init (PPID 1).
fn count_orphaned_a11y_daemons(procs: &[ProcEntry]) -> usize {
    procs
        .iter()
        .filter(|p| {
            p.comm == "dbus-daemon" && p.cmdline.contains("accessibility.conf") && p.ppid == 1
        })
        .count()
}

/// Pure: build the a11y checks from gathered facts.
fn a11y_checks(launcher_installed: bool, facts: &HostA11yFacts) -> Vec<Check> {
    let mut checks = Vec::new();

    // Concern A — can glass do a11y AT ALL? Honest precondition, never a "will work" promise.
    checks.push(if launcher_installed {
        Check::new(
            "a11y",
            CheckStatus::Ok,
            "at-spi-bus-launcher present — glass spawns a private a11y bus on a11y:true launches. \
             Whether a given window exposes an accessibility tree is up to the app (egui/GTK/Qt \
             expose it; games/canvas apps may not); glass_a11y_snapshot reports per app.",
        )
    } else {
        Check::new("a11y", CheckStatus::Warn, "at-spi-bus-launcher not found").with_remedy(
            "install the AT-SPI registry (e.g. `apt install at-spi2-core`) so glass can spawn its private a11y bus",
        )
    });

    // Concern B — host desktop a11y health (#9). Detect-only; never mutate.
    checks.push(match &facts.bus {
        HostBusState::Reachable => {
            Check::new("host desktop a11y", CheckStatus::Ok, "your desktop accessibility bus is healthy")
        }
        HostBusState::Wedged { address } => Check::new(
            "host desktop a11y",
            CheckStatus::Warn,
            format!("your desktop a11y bus is wedged — it advertises {address} but the socket won't connect"),
        )
        .with_remedy(
            "the a11y daemon is alive but its socket got unlinked. Restart at-spi: kill the \
             at-spi-bus-launcher / at-spi2-registryd / a11y dbus-daemon processes by PID, then \
             re-activate with any a11y client (`dbus-send --session --dest=org.a11y.Bus \
             --print-reply /org/a11y/bus org.a11y.Bus.GetAddress`). glass's own a11y is \
             unaffected (it uses a private bus).",
        ),
        HostBusState::NotRunning => Check::new(
            "host desktop a11y",
            CheckStatus::Ok,
            "no desktop a11y bus running (normal on a headless box; glass's private bus is unaffected)",
        ),
    });

    // Concern B — leaked/orphaned a11y daemons (only surfaced when present).
    if facts.orphaned_daemons > 0 {
        checks.push(
            Check::new(
                "leaked a11y daemons",
                CheckStatus::Warn,
                format!(
                    "{} orphaned a11y dbus-daemon(s) (reparented to init) — likely leaked by a prior \
                     glass run before isolation, or an unrelated wedge; they can wedge your desktop a11y",
                    facts.orphaned_daemons
                ),
            )
            .with_remedy("kill the orphaned a11y dbus-daemon(s) by PID, then re-activate at-spi as above"),
        );
    }

    // Retained as host-environment context (needed to query the host bus above).
    checks.push(if facts.session_bus {
        Check::new("session bus", CheckStatus::Ok, "DBUS_SESSION_BUS_ADDRESS is set")
    } else {
        Check::new("session bus", CheckStatus::Warn, "DBUS_SESSION_BUS_ADDRESS unset").with_remedy(
            "a session D-Bus is needed to assess host a11y health; run inside a desktop session or `dbus-run-session`",
        )
    });

    checks
}

/// The at-spi bus launcher across known libexec/lib locations.
fn find_registry() -> Option<std::path::PathBuf> {
    const CANDIDATES: &[&str] = &[
        "/usr/libexec/at-spi-bus-launcher",
        "/usr/lib/at-spi2-core/at-spi-bus-launcher",
        "/usr/lib/at-spi2/at-spi-bus-launcher",
    ];
    find_launcher(CANDIDATES, "/usr/lib")
}

/// Find the launcher among `fixed` absolute paths, then (if none hit) by scanning
/// `multiarch_root/<triplet>/at-spi2-core/at-spi-bus-launcher`. The triplet directory is
/// arch-specific (`x86_64-linux-gnu`, `aarch64-linux-gnu`, …), so scanning rather than
/// hardcoding one arch keeps the AT-SPI-present signal correct on non-x86_64 hosts.
fn find_launcher(fixed: &[&str], multiarch_root: &str) -> Option<std::path::PathBuf> {
    if let Some(p) = fixed
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.is_file())
    {
        return Some(p);
    }
    std::fs::read_dir(multiarch_root)
        .ok()?
        .flatten()
        .map(|e| e.path().join("at-spi2-core/at-spi-bus-launcher"))
        .find(|p| p.is_file())
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
    use glass_core::capability::Support;

    // ---- capability signal (shared with glass-x11 / glass-wayland `capabilities()`) ----
    #[test]
    fn launcher_present_predicate_matches_find_registry() {
        // The capability signal must be the *same* fact the doctor's head check reads —
        // one source, so `glass_capabilities` and `glass doctor` can't disagree.
        assert_eq!(accessibility_launcher_present(), find_registry().is_some());
    }

    #[test]
    fn accessibility_capability_supported_when_launcher_present() {
        let c = accessibility_capability(true);
        assert_eq!(c.status, Support::Supported);
        assert!(c.note.is_none());
    }

    #[test]
    fn accessibility_capability_requires_setup_when_launcher_absent() {
        let c = accessibility_capability(false);
        assert_eq!(c.status, Support::RequiresSetup);
        assert!(c.note.unwrap().contains("at-spi2-core"));
    }

    // ---- launcher discovery (impure FS scan, driven with a tempdir) ----
    #[test]
    fn find_launcher_prefers_a_present_fixed_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let fixed = dir.path().join("at-spi-bus-launcher");
        std::fs::write(&fixed, b"").unwrap();
        let fixed_str = fixed.to_str().unwrap();
        assert_eq!(
            find_launcher(&[fixed_str], "/nonexistent-root"),
            Some(fixed)
        );
    }

    #[test]
    fn find_launcher_scans_any_multiarch_triplet_dir() {
        // The launcher under an arbitrary <triplet>/at-spi2-core/ dir must be found — not just
        // x86_64 (regression guard: an aarch64 host must not report AT-SPI missing).
        let root = tempfile::tempdir().unwrap();
        let launcher = root
            .path()
            .join("aarch64-linux-gnu/at-spi2-core/at-spi-bus-launcher");
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        std::fs::write(&launcher, b"").unwrap();
        assert_eq!(
            find_launcher(&[], root.path().to_str().unwrap()),
            Some(launcher)
        );
    }

    #[test]
    fn find_launcher_none_when_absent() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(find_launcher(&[], root.path().to_str().unwrap()), None);
    }

    // ---- pure helpers ----
    #[test]
    fn socket_path_parsed_from_address() {
        assert_eq!(
            socket_path_from_address("unix:path=/run/user/1000/at-spi/bus_0,guid=abc"),
            Some("/run/user/1000/at-spi/bus_0")
        );
        assert_eq!(socket_path_from_address("unix:abstract=/tmp/x"), None);
    }

    #[test]
    fn classify_bus_states() {
        assert_eq!(
            classify_host_bus(true, Some("unix:path=/x"), false),
            HostBusState::Reachable
        );
        assert_eq!(
            classify_host_bus(false, Some("unix:path=/x"), false),
            HostBusState::Wedged {
                address: "unix:path=/x".into()
            }
        );
        assert_eq!(
            classify_host_bus(false, Some("unix:path=/x"), true),
            HostBusState::NotRunning
        );
        assert_eq!(
            classify_host_bus(false, None, false),
            HostBusState::NotRunning
        );
    }

    #[test]
    fn ppid_parsed_after_comm_with_parens_and_spaces() {
        assert_eq!(
            parse_ppid_from_stat("42 (we (ird) proc) S 1 999 x"),
            Some(1)
        );
        assert_eq!(
            parse_ppid_from_stat("7 (dbus-daemon) R 1234 7 x"),
            Some(1234)
        );
        assert_eq!(parse_ppid_from_stat("garbage"), None);
    }

    #[test]
    fn counts_only_orphaned_a11y_daemons() {
        let procs = vec![
            ProcEntry {
                comm: "dbus-daemon".into(),
                cmdline: "dbus-daemon --config-file=/usr/share/defaults/at-spi2/accessibility.conf"
                    .into(),
                ppid: 1,
            },
            ProcEntry {
                comm: "dbus-daemon".into(),
                cmdline: "dbus-daemon --config-file=/x/accessibility.conf".into(),
                ppid: 5000,
            },
            ProcEntry {
                comm: "dbus-daemon".into(),
                cmdline: "dbus-daemon --session".into(),
                ppid: 1,
            },
            ProcEntry {
                comm: "at-spi-bus-launcher".into(),
                cmdline: "x".into(),
                ppid: 1,
            },
        ];
        assert_eq!(count_orphaned_a11y_daemons(&procs), 1);
    }

    // ---- pure mapper ----
    fn facts(bus: HostBusState, orphaned: usize) -> HostA11yFacts {
        HostA11yFacts {
            session_bus: true,
            bus,
            orphaned_daemons: orphaned,
        }
    }

    #[test]
    fn launcher_present_states_precondition_not_a_promise() {
        let cs = a11y_checks(true, &facts(HostBusState::Reachable, 0));
        let head = cs.iter().find(|c| c.name == "a11y").unwrap();
        assert_eq!(head.status, CheckStatus::Ok);
        assert!(head.detail.contains("private a11y bus"));
        assert!(!head.detail.contains("will work"));
    }

    #[test]
    fn launcher_absent_warns_with_install_remedy() {
        let cs = a11y_checks(false, &facts(HostBusState::NotRunning, 0));
        let head = cs.iter().find(|c| c.name == "a11y").unwrap();
        assert_eq!(head.status, CheckStatus::Warn);
        assert!(head.remedy.is_some());
    }

    #[test]
    fn wedged_host_bus_warns() {
        let cs = a11y_checks(
            true,
            &facts(
                HostBusState::Wedged {
                    address: "unix:path=/x".into(),
                },
                0,
            ),
        );
        let h = cs.iter().find(|c| c.name == "host desktop a11y").unwrap();
        assert_eq!(h.status, CheckStatus::Warn);
        assert!(h.remedy.is_some());
    }

    #[test]
    fn healthy_host_bus_ok_and_no_leak_warning() {
        let cs = a11y_checks(true, &facts(HostBusState::Reachable, 0));
        assert_eq!(
            cs.iter()
                .find(|c| c.name == "host desktop a11y")
                .unwrap()
                .status,
            CheckStatus::Ok
        );
        assert!(cs.iter().all(|c| c.name != "leaked a11y daemons"));
    }

    #[test]
    fn leaked_daemons_warn_with_count() {
        let cs = a11y_checks(true, &facts(HostBusState::NotRunning, 3));
        let leak = cs.iter().find(|c| c.name == "leaked a11y daemons").unwrap();
        assert_eq!(leak.status, CheckStatus::Warn);
        assert!(leak.detail.contains('3'));
        assert!(leak.remedy.is_some());
    }
}
