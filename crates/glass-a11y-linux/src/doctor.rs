//! Environment checks for the Linux accessibility backend ("glass doctor"). The
//! pure `a11y_checks` maps gathered facts to `Check`s and is unit-tested without a
//! bus; `checks` gathers the real environment — including a **live probe of the
//! accessibility bus**, because the launcher being installed and the session-bus var
//! being set do NOT mean the a11y bus is actually running (GNOME starts it lazily,
//! only once an AT client enables it). Without that probe, doctor would report a
//! green "ready" while `glass_a11y_*` calls fail at runtime.

use std::sync::mpsc;
use std::time::Duration;

use glass_core::capability::CapabilityStatus;
use glass_core::{Check, CheckStatus, ProbeFailure};
use glass_exec_unix::Resolved;

/// How long either host-bus probe may take before doctor stops waiting on it.
const PROBE_BUDGET: Duration = Duration::from_secs(3);

/// The one action every unhealthy host-bus state calls for. Its diagnosis stays in each check's
/// detail: "its socket got unlinked" is wrong for a bus that never answered at all.
const RESTART_AT_SPI: &str = "restart at-spi: kill the at-spi-bus-launcher / at-spi2-registryd / \
     a11y dbus-daemon processes by PID, then re-activate with any a11y client (`dbus-send \
     --session --dest=org.a11y.Bus --print-reply /org/a11y/bus org.a11y.Bus.GetAddress`). glass's \
     own a11y is unaffected (it uses a private bus).";

/// What a check that never ran calls for. Not [`ProbeFailure::remedy`]'s text: its "full or
/// read-only temp dir" is a cause for the deep probes that lay one out; this one needs a thread
/// and a runtime.
const COULD_NOT_ASK: &str = "nothing here is about your desktop bus — glass never got to ask it, \
     and the detail says what stopped it: a `pids` cgroup limit low enough to refuse a thread, or \
     an fd limit low enough to refuse a runtime. A probe that panicked leaves the panic on glass's \
     stderr.";

/// Live: is the AT-SPI bus launcher installed, so glass can spawn its private a11y bus?
/// This is the desktop-a11y capability signal for the Linux backends — the *same* fact the
/// doctor's head "a11y" check reads, so `glass_capabilities` and `glass doctor` can't drift. It is
/// only a precondition ("glass can do a11y at all"), never a promise that a given window exposes a
/// tree — that's up to the app.
///
/// The lookup belongs to `glass-dbus-linux`, the crate that spawns the launcher: reporting on a
/// binary a different lookup would resolve is how one run came to answer both "present" and
/// "not found" about one file (glass#391).
pub fn accessibility_launcher_present() -> bool {
    launcher_present(glass_dbus_linux::find_launcher())
}

/// Only a runnable launcher counts as present — what this feeds says a11y is ready.
fn launcher_present(resolved: Resolved) -> bool {
    matches!(resolved, Resolved::Found(_))
}

/// The desktop-`accessibility` capability cell for a Linux backend, from the launcher-present
/// signal. Shared by glass-x11 and glass-wayland (identical stacks) so their note can't drift.
pub const fn accessibility_capability(launcher_present: bool) -> CapabilityStatus {
    if launcher_present {
        CapabilityStatus::supported()
    } else {
        // Not "AT-SPI is not installed": the launcher can also be there and unrunnable, or a
        // GLASS_ATSPI_LAUNCHER the operator set can name nothing. `glass doctor` tells them apart.
        CapabilityStatus::requires_setup(
            "no runnable at-spi-bus-launcher, so glass cannot spawn its private a11y bus; install \
             at-spi2-core (or check GLASS_ATSPI_LAUNCHER if you set it) and see `glass doctor`",
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

/// Run `work` on a private thread with its own current-thread runtime, and wait `budget` for it.
///
/// A private thread because the reader drives the same async API and `block_on` panics inside the
/// caller's runtime, and because a bus that never answers must not hang doctor with it. The thread
/// is detached: a wait that ends does not end the work, and a worker that died is not a wait that
/// elapsed (glass#455).
fn probe_on_a_private_runtime<T, Fut>(
    budget: Duration,
    work: impl FnOnce() -> Fut + Send + 'static,
) -> Result<T, ProbeFailure>
where
    T: Send + 'static,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let (tx, rx) = mpsc::channel();
    // Builder, not `thread::spawn`: a host that refuses the thread panics the caller there.
    let spawned = std::thread::Builder::new()
        .name("glass-doctor-a11y".into())
        .spawn(move || {
            let res = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(work()).map_err(ProbeFailure::Failed),
                Err(e) => Err(ProbeFailure::NotStarted(e.to_string())),
            };
            let _ = tx.send(res);
        });
    if let Err(e) = spawned {
        return Err(ProbeFailure::NotStarted(e.to_string()));
    }
    match rx.recv_timeout(budget) {
        Ok(res) => res,
        Err(e) => Err(ProbeFailure::from_recv(e, budget)),
    }
}

/// Ask the host session bus what a11y address it advertises (`org.a11y.Bus.GetAddress`).
fn advertised_a11y_address() -> Result<String, ProbeFailure> {
    probe_on_a_private_runtime(PROBE_BUDGET, || async {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| e.to_string())?;
        let proxy = zbus::Proxy::new(&conn, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
            .await
            .map_err(|e| e.to_string())?;
        proxy
            .call_method("GetAddress", &())
            .await
            .map_err(|e| e.to_string())?
            .body()
            .deserialize::<String>()
            .map_err(|e| e.to_string())
    })
}

/// Gather host a11y facts (impure: live probe + GetAddress + `/proc`). Read-only — never mutates.
fn gather_host_a11y() -> HostA11yFacts {
    let session_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    let connect = probe_a11y_bus();
    let advertised = advertised_a11y_address();
    let socket_present = advertised
        .as_deref()
        .ok()
        .and_then(socket_path_from_address)
        .is_some_and(|p| std::path::Path::new(p).exists());
    let bus = classify_host_bus(connect, advertised, socket_present);
    let orphaned_daemons = count_orphaned_a11y_daemons(&read_proc_entries());
    HostA11yFacts {
        session_bus,
        bus,
        orphaned_daemons,
    }
}

/// Probe whether the AT-SPI accessibility stack is usable.
pub fn checks() -> Vec<Check> {
    a11y_checks(
        &glass_dbus_linux::find_launcher(),
        atspi_launcher_override_set(),
        &gather_host_a11y(),
    )
}

/// Whether `GLASS_ATSPI_LAUNCHER` holds a non-empty value — the same condition under which
/// `find_launcher` skips discovery, so a failed lookup can name the variable as its cause.
fn atspi_launcher_override_set() -> bool {
    std::env::var_os("GLASS_ATSPI_LAUNCHER").is_some_and(|v| !v.is_empty())
}

/// Health of the *host* (operator's desktop) AT-SPI bus — distinct from glass's private bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostBusState {
    /// glass connected to it.
    Reachable,
    /// It advertises an address glass could not connect to.
    Wedged {
        address: String,
        socket_present: bool,
    },
    /// It took the `GetAddress` call and never answered — the one host state this check exists to
    /// surface, and the one that used to read as a green "no bus running" (glass#455).
    Unresponsive { waited: Duration },
    /// Nothing was learned about the host: the probe never started, or its thread unwound. Only
    /// those two [`ProbeFailure`]s reach here — the other two are answers about the bus itself.
    Unaskable(ProbeFailure),
    /// The session bus answered, and no a11y bus is running.
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

/// Classify the host bus from the connection attempt and what (if anything) the session bus
/// advertises.
///
/// The connection attempt is read first for the two outcomes that are about glass's own host
/// rather than the bus: an advertised address says nothing about connectivity when nothing got as
/// far as connecting. A `GetAddress` that answers cleanly then settles it — including the answer
/// that there is no bus, which is why a connection that merely timed out does not overrule it.
fn classify_host_bus(
    connect: Result<(), ProbeFailure>,
    advertised: Result<String, ProbeFailure>,
    socket_present: bool,
) -> HostBusState {
    match connect {
        Ok(()) => return HostBusState::Reachable,
        Err(nothing_learned @ (ProbeFailure::NotStarted(_) | ProbeFailure::Vanished)) => {
            return HostBusState::Unaskable(nothing_learned);
        }
        Err(ProbeFailure::Failed(_) | ProbeFailure::TimedOut(_)) => {}
    }
    match advertised {
        // The launcher owns `org.a11y.Bus` before it has a bus to name, and answers "" until then.
        Ok(address) if !address.is_empty() => HostBusState::Wedged {
            address,
            socket_present,
        },
        Err(ProbeFailure::TimedOut(waited)) => HostBusState::Unresponsive { waited },
        Err(nothing_learned @ (ProbeFailure::NotStarted(_) | ProbeFailure::Vanished)) => {
            HostBusState::Unaskable(nothing_learned)
        }
        Ok(_) | Err(ProbeFailure::Failed(_)) => HostBusState::NotRunning,
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
fn a11y_checks(launcher: &Resolved, override_set: bool, facts: &HostA11yFacts) -> Vec<Check> {
    let mut checks = Vec::new();

    // Concern A — can glass do a11y AT ALL? Honest precondition, never a "will work" promise.
    // Each way of having no launcher gets its own remedy — "install at-spi2-core" about a file
    // that is already there sends the user to fix what is not broken.
    checks.push(match launcher {
        Resolved::Found(_) => Check::new(
            "a11y",
            CheckStatus::Ok,
            "at-spi-bus-launcher present — glass spawns a private a11y bus on a11y:true launches. \
             Whether a given window exposes an accessibility tree is up to the app (egui/GTK/Qt \
             expose it; games/canvas apps may not); glass_a11y_snapshot reports per app.",
        ),
        Resolved::NotExecutable(p) => Check::new(
            "a11y",
            CheckStatus::Warn,
            format!(
                "at-spi-bus-launcher at {} is present but not executable",
                p.display()
            ),
        )
        .with_remedy(
            "restore its execute bit (`chmod +x`), or point GLASS_ATSPI_LAUNCHER at a runnable copy",
        ),
        // An override skips discovery outright, so the well-known paths were never consulted:
        // whatever this host has installed, the variable is what left glass with nothing.
        Resolved::Absent if override_set => Check::new(
            "a11y",
            CheckStatus::Warn,
            "GLASS_ATSPI_LAUNCHER does not name a runnable at-spi-bus-launcher",
        )
        .with_remedy(
            "point GLASS_ATSPI_LAUNCHER at a runnable launcher, or unset it to search the \
             well-known install paths",
        ),
        Resolved::Absent => Check::new("a11y", CheckStatus::Warn, "at-spi-bus-launcher not found").with_remedy(
            "install the AT-SPI registry (e.g. `apt install at-spi2-core`) so glass can spawn its private a11y bus",
        ),
        // Unreachable while discovery walks fixed paths — kept apart because a launcher that was
        // never looked up is not one that is missing (glass#373).
        Resolved::NoSearchPath => Check::new(
            "a11y",
            CheckStatus::Warn,
            "at-spi-bus-launcher could not be looked up — PATH is unset in glass's environment",
        )
        // No PATH advice: discovery walks fixed paths and would not read one.
        .with_remedy("point GLASS_ATSPI_LAUNCHER at the launcher"),
    });

    // Concern B — host desktop a11y health (#9). Detect-only; never mutate.
    checks.push(match &facts.bus {
        HostBusState::Reachable => {
            Check::new("host desktop a11y", CheckStatus::Ok, "your desktop accessibility bus is healthy")
        }
        HostBusState::Wedged { address, socket_present } => Check::new(
            "host desktop a11y",
            CheckStatus::Warn,
            format!(
                "your desktop a11y bus is wedged — it advertises {address} and glass could not \
                 connect to it: {}",
                if *socket_present {
                    "its socket file is still there"
                } else {
                    "its socket file is gone — the daemon is alive with its socket unlinked"
                }
            ),
        )
        .with_remedy(RESTART_AT_SPI),
        HostBusState::Unresponsive { waited } => Check::new(
            "host desktop a11y",
            CheckStatus::Warn,
            format!(
                "your desktop a11y bus did not answer GetAddress within {waited:?} — something \
                 owns org.a11y.Bus and is not replying, so host a11y clients hang on it"
            ),
        )
        .with_remedy(RESTART_AT_SPI),
        HostBusState::Unaskable(why) => Check::new(
            "host desktop a11y",
            CheckStatus::Warn,
            why.detail("host a11y bus"),
        )
        .with_remedy(COULD_NOT_ASK),
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

/// Try to reach the accessibility bus exactly the way the reader does. `Ok(())` means a
/// connection was established and dropped.
fn probe_a11y_bus() -> Result<(), ProbeFailure> {
    probe_on_a_private_runtime(PROBE_BUDGET, || async {
        atspi::connection::AccessibilityConnection::new()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::capability::Support;

    // ---- capability signal (shared with glass-x11 / glass-wayland `capabilities()`) ----
    #[test]
    fn launcher_present_when_the_spawner_resolved_one() {
        assert!(launcher_present(Resolved::Found("/usr/libexec/x".into())));
    }

    /// A launcher glass cannot spawn would make the ready cell a promise the launch breaks, so it
    /// counts as absent here.
    #[test]
    fn launcher_absent_when_present_but_unrunnable() {
        assert!(!launcher_present(Resolved::NotExecutable(
            "/usr/libexec/x".into()
        )));
    }

    #[test]
    fn launcher_absent_when_nothing_resolved() {
        assert!(!launcher_present(Resolved::Absent));
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

    // ---- pure helpers ----
    #[test]
    fn socket_path_parsed_from_address() {
        assert_eq!(
            socket_path_from_address("unix:path=/run/user/1000/at-spi/bus_0,guid=abc"),
            Some("/run/user/1000/at-spi/bus_0")
        );
        assert_eq!(socket_path_from_address("unix:abstract=/tmp/x"), None);
    }

    // ---- classification ----
    /// An answer from the bus itself, for the cases that turn on what the *other* probe found.
    fn answered(what: &str) -> ProbeFailure {
        ProbeFailure::Failed(what.into())
    }

    fn wedged(socket_present: bool) -> HostBusState {
        HostBusState::Wedged {
            address: "unix:path=/x".into(),
            socket_present,
        }
    }

    #[test]
    fn a_bus_glass_connected_to_is_reachable() {
        assert_eq!(
            classify_host_bus(Ok(()), Ok("unix:path=/x".into()), true),
            HostBusState::Reachable
        );
    }

    #[test]
    fn an_advertised_bus_that_will_not_connect_is_wedged() {
        assert_eq!(
            classify_host_bus(
                Err(answered("Connection refused")),
                Ok("unix:path=/x".into()),
                false
            ),
            wedged(false)
        );
    }

    /// A socket that is still there does not make an unconnectable bus a host without one: the
    /// daemon holds it open and does not serve it. That fell to the green arm (glass#455).
    #[test]
    fn an_advertised_bus_is_still_wedged_when_its_socket_is_there() {
        assert_eq!(
            classify_host_bus(
                Err(answered("Connection refused")),
                Ok("unix:path=/x".into()),
                true
            ),
            wedged(true)
        );
    }

    /// The one this check exists for: a bus that takes `GetAddress` and never answers used to
    /// land in the arm that says nothing is wrong (glass#455).
    #[test]
    fn a_bus_that_never_answered_is_not_a_host_without_one() {
        assert_eq!(
            classify_host_bus(
                Err(ProbeFailure::TimedOut(PROBE_BUDGET)),
                Err(ProbeFailure::TimedOut(PROBE_BUDGET)),
                false
            ),
            HostBusState::Unresponsive {
                waited: PROBE_BUDGET
            }
        );
    }

    /// The launcher owns `org.a11y.Bus` before it has a bus to name, and answers `""` until then.
    /// Calling that a wedge prints a remedy about an address that names nothing.
    #[test]
    fn an_empty_address_is_no_bus_rather_than_a_wedged_one() {
        assert_eq!(
            classify_host_bus(Err(answered("no address")), Ok(String::new()), false),
            HostBusState::NotRunning
        );
    }

    #[test]
    fn a_session_bus_that_answered_no_a11y_bus_is_not_running_one() {
        assert_eq!(
            classify_host_bus(
                Err(answered("ServiceUnknown")),
                Err(answered("org.freedesktop.DBus.Error.ServiceUnknown")),
                false
            ),
            HostBusState::NotRunning
        );
    }

    /// A probe the host refused learned nothing about the bus, so an advertised address is not
    /// evidence of a wedge — nothing tried to connect.
    #[test]
    fn a_probe_that_never_ran_is_not_a_verdict_on_the_host() {
        let refused = ProbeFailure::NotStarted("Resource temporarily unavailable".into());
        assert_eq!(
            classify_host_bus(Err(refused.clone()), Ok("unix:path=/x".into()), true),
            HostBusState::Unaskable(refused)
        );
    }

    #[test]
    fn a_getaddress_worker_that_unwound_is_not_a_host_without_a_bus() {
        assert_eq!(
            classify_host_bus(
                Err(answered("Connection refused")),
                Err(ProbeFailure::Vanished),
                false
            ),
            HostBusState::Unaskable(ProbeFailure::Vanished)
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
    /// A launcher the spawner resolved, for the cases that are about the host bus, not the lookup.
    fn found() -> Resolved {
        Resolved::Found("/usr/libexec/at-spi-bus-launcher".into())
    }

    fn facts(bus: HostBusState, orphaned: usize) -> HostA11yFacts {
        HostA11yFacts {
            session_bus: true,
            bus,
            orphaned_daemons: orphaned,
        }
    }

    #[test]
    fn launcher_present_states_precondition_not_a_promise() {
        let cs = a11y_checks(&found(), false, &facts(HostBusState::Reachable, 0));
        let head = cs.iter().find(|c| c.name == "a11y").unwrap();
        assert_eq!(head.status, CheckStatus::Ok);
        assert!(head.detail.contains("private a11y bus"));
        assert!(!head.detail.contains("will work"));
    }

    #[test]
    fn launcher_absent_warns_with_install_remedy() {
        let cs = a11y_checks(
            &Resolved::Absent,
            false,
            &facts(HostBusState::NotRunning, 0),
        );
        let head = cs.iter().find(|c| c.name == "a11y").unwrap();
        assert_eq!(head.status, CheckStatus::Warn);
        assert!(head.remedy.is_some());
    }

    /// "apt install at-spi2-core" about a file that is right there sends the user to fix what is
    /// not broken. The spawn path has always named it; this is the surface that did not.
    #[test]
    fn launcher_present_but_unrunnable_names_the_file_not_the_package() {
        let cs = a11y_checks(
            &Resolved::NotExecutable("/usr/libexec/at-spi-bus-launcher".into()),
            false,
            &facts(HostBusState::NotRunning, 0),
        );
        let head = cs.iter().find(|c| c.name == "a11y").unwrap();
        assert_eq!(head.status, CheckStatus::Warn);
        assert!(head.detail.contains("/usr/libexec/at-spi-bus-launcher"));
        assert!(!head.remedy.clone().unwrap().contains("at-spi2-core"));
    }

    /// An override skips discovery, so a wrong one leaves glass with nothing on a host where
    /// at-spi2-core is installed and fine. Naming the package there is the same misdirection.
    #[test]
    fn a_wrong_override_names_the_variable_not_the_package() {
        let cs = a11y_checks(&Resolved::Absent, true, &facts(HostBusState::NotRunning, 0));
        let head = cs.iter().find(|c| c.name == "a11y").unwrap();
        assert_eq!(head.status, CheckStatus::Warn);
        assert!(head.detail.contains("GLASS_ATSPI_LAUNCHER"));
        assert!(!head.remedy.clone().unwrap().contains("at-spi2-core"));
    }

    /// glass#373: unreachable while the launcher is looked for by path — kept distinct so a
    /// caller that searches `$PATH` one day does not inherit "install at-spi2-core".
    #[test]
    fn a_launcher_that_could_not_be_looked_up_is_not_reported_as_missing() {
        let cs = a11y_checks(
            &Resolved::NoSearchPath,
            false,
            &facts(HostBusState::NotRunning, 0),
        );
        let head = cs.iter().find(|c| c.name == "a11y").unwrap();
        assert_eq!(head.status, CheckStatus::Warn);
        assert!(head.detail.contains("PATH"), "{:?}", head.detail);
        assert!(
            !head.remedy.clone().unwrap().contains("at-spi2-core"),
            "the package may well be installed: {:?}",
            head.remedy
        );
    }

    /// The one "host desktop a11y" check, for the states that differ only in what it prints.
    fn host_bus(checks: &[Check]) -> &Check {
        checks
            .iter()
            .find(|c| c.name == "host desktop a11y")
            .unwrap()
    }

    fn reported(bus: HostBusState) -> Check {
        host_bus(&a11y_checks(&found(), false, &facts(bus, 0))).clone()
    }

    #[test]
    fn wedged_host_bus_warns() {
        let h = reported(wedged(false));
        assert_eq!(h.status, CheckStatus::Warn);
        assert!(h.remedy.is_some());
    }

    /// The socket being gone and the socket being held open are different things to have found,
    /// and the operator reads the detail to tell which one they are looking at.
    #[test]
    fn a_wedged_bus_says_whether_its_socket_is_still_there() {
        let gone = reported(wedged(false)).detail;
        let there = reported(wedged(true)).detail;
        assert_ne!(gone, there);
        assert!(gone.contains("unix:path=/x"), "{gone}");
        assert!(there.contains("unix:path=/x"), "{there}");
    }

    /// glass#455: reported in green as the state where nothing is wrong, this was the one host
    /// failure the check was added to catch.
    #[test]
    fn a_hung_host_bus_warns_and_says_how_long_it_waited() {
        let h = reported(HostBusState::Unresponsive {
            waited: Duration::from_secs(3),
        });
        assert_eq!(h.status, CheckStatus::Warn);
        assert!(h.detail.contains("3s"), "{}", h.detail);
        assert!(h.remedy.is_some());
    }

    /// A check that never ran says what stopped it, and does not send the operator to restart a
    /// desktop stack nothing has looked at.
    #[test]
    fn a_check_that_could_not_run_does_not_prescribe_a_restart() {
        let h = reported(HostBusState::Unaskable(ProbeFailure::NotStarted(
            "Resource temporarily unavailable".into(),
        )));
        assert_eq!(h.status, CheckStatus::Warn);
        assert!(
            h.detail.contains("Resource temporarily unavailable"),
            "{}",
            h.detail
        );
        let remedy = h.remedy.clone().unwrap();
        assert!(!remedy.contains("restart at-spi"), "{remedy}");
        // Sending an operator to check `/tmp` over an `EAGAIN` is this change's own defect.
        assert!(!remedy.contains("temp dir"), "{remedy}");
    }

    /// Each state is a different thing to do about it, so no two may print the same line.
    #[test]
    fn every_host_bus_state_says_something_different() {
        let details = [
            HostBusState::Reachable,
            wedged(false),
            wedged(true),
            HostBusState::Unresponsive {
                waited: Duration::from_secs(3),
            },
            HostBusState::Unaskable(ProbeFailure::Vanished),
            HostBusState::NotRunning,
        ]
        .map(|bus| reported(bus).detail);
        let unique: std::collections::BTreeSet<&String> = details.iter().collect();
        assert_eq!(unique.len(), details.len(), "{details:#?}");
    }

    // ---- the probe seam both host-bus probes run through ----
    /// A call the bus answered with an error is the bus's answer, not a wait that ran out — the
    /// distinction the old `Err(_) => "timed out"` threw away.
    #[test]
    fn a_probe_that_failed_is_not_reported_as_one_that_timed_out() {
        let out: Result<(), ProbeFailure> =
            probe_on_a_private_runtime(Duration::from_secs(30), || async {
                Err::<(), String>("bus said no".into())
            });
        assert_eq!(out, Err(ProbeFailure::Failed("bus said no".into())));
    }

    /// The budget the wait was given is the one it reports, so "3s" is never a number from a
    /// constant somewhere else.
    #[test]
    fn a_probe_that_outlives_its_budget_times_out_against_that_budget() {
        let budget = Duration::from_millis(50);
        let out: Result<(), ProbeFailure> =
            probe_on_a_private_runtime(budget, move || async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            });
        assert_eq!(out, Err(ProbeFailure::TimedOut(budget)));
    }

    #[test]
    fn healthy_host_bus_ok_and_no_leak_warning() {
        let cs = a11y_checks(&found(), false, &facts(HostBusState::Reachable, 0));
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
        let cs = a11y_checks(&found(), false, &facts(HostBusState::NotRunning, 3));
        let leak = cs.iter().find(|c| c.name == "leaked a11y daemons").unwrap();
        assert_eq!(leak.status, CheckStatus::Warn);
        assert!(leak.detail.contains('3'));
        assert!(leak.remedy.is_some());
    }
}
