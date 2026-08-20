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

/// How long each host-bus probe may take before doctor stops waiting on it.
pub const PROBE_BUDGET: Duration = Duration::from_secs(3);

/// The action the states that found a bus and could not use it call for. Its diagnosis stays in
/// each check's detail — "its socket got unlinked" is wrong for a bus that never answered.
/// The PIDs are the host's — glass's own private bus is an `at-spi-bus-launcher` too.
const RESTART_AT_SPI: &str = "restart the host's at-spi: kill its at-spi-bus-launcher / \
     at-spi2-registryd / a11y dbus-daemon processes by PID, then re-activate with any a11y client \
     (`dbus-send --session --dest=org.a11y.Bus --print-reply /org/a11y/bus \
     org.a11y.Bus.GetAddress`). glass's own a11y is unaffected (it uses a private bus).";

/// What a question the session bus answered with an error calls for.
const ASK_FAILED: &str = "the error is the session bus's own — reproduce it with `dbus-send \
     --session --dest=org.a11y.Bus --print-reply /org/a11y/bus org.a11y.Bus.GetAddress`. An \
     activation failure there means at-spi-bus-launcher is failing to start. glass's own a11y is \
     unaffected (it uses a private bus).";

/// What a check that produced no answer of its own calls for. Not [`ProbeFailure::remedy`]'s
/// text: its "full or read-only temp dir" is a cause for the deep probes that lay one out, and
/// this one needs only a thread and a runtime.
const COULD_NOT_ASK: &str = "nothing here is about your desktop bus — glass got no answer of its \
     own. The detail names the cause when the host refused the probe (a `pids` cgroup limit, or an \
     fd limit low enough to refuse a runtime); a probe that panicked leaves the panic on glass's \
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
/// A private thread because `block_on` panics inside the caller's runtime, which the reader also
/// drives. The thread is detached: a wait that ends does not end the work (glass#455).
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

/// The D-Bus error for a name nothing serves and nothing can start — what a host with no desktop
/// accessibility stack answers.
const NO_SUCH_SERVICE: &str = "org.freedesktop.DBus.Error.ServiceUnknown";

/// Ask the host session bus what a11y address it advertises (`org.a11y.Bus.GetAddress`).
///
/// `Ok(None)` is the settled answer that there is no host bus; an error means the question could
/// not be answered (glass#455).
fn advertised_a11y_address() -> Result<Option<String>, ProbeFailure> {
    probe_on_a_private_runtime(PROBE_BUDGET, || async {
        // No session bus is no desktop a11y bus. The "session bus" check below reports the cause.
        let Ok(conn) = zbus::Connection::session().await else {
            return Ok(None);
        };
        let proxy = zbus::Proxy::new(&conn, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
            .await
            .map_err(|e| e.to_string())?;
        let reply = match proxy.call_method("GetAddress", &()).await {
            Ok(reply) => reply,
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == NO_SUCH_SERVICE => {
                return Ok(None);
            }
            Err(e) => return Err(e.to_string()),
        };
        let address = reply
            .body()
            .deserialize::<String>()
            .map_err(|e| e.to_string())?;
        // The launcher owns `org.a11y.Bus` before it has a bus to name, and answers "" until then.
        Ok((!address.is_empty()).then_some(address))
    })
}

/// Gather host a11y facts (impure: live probe + GetAddress + `/proc`). Read-only — never mutates.
///
/// The address is asked for first because `AccessibilityConnection::new` asks for it too
/// (atspi-connection 0.14): probing the connection first reports a bus still warming up as one
/// that cannot be connected to.
fn gather_host_a11y() -> HostA11yFacts {
    let session_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    let bus = classify_host_bus(advertised_a11y_address(), probe_a11y_bus, socket_state);
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
    /// It advertises an address glass could not use, `why` saying which way the attempt ended.
    Wedged {
        address: String,
        /// Whether the address's socket file exists — `None` when the address names no file to
        /// look for, which is not the same as looking and finding none.
        socket: Option<bool>,
        why: String,
    },
    /// It took the `GetAddress` call and never answered — which used to read as a green "no bus
    /// running" (glass#455).
    Unresponsive { waited: Duration },
    /// The session bus answered the question with an error that does not mean "nothing serves that
    /// name", so whether a bus is there is unknown.
    Unknown { why: String },
    /// glass's own probe produced no answer — it never started, or its thread unwound. Only
    /// those two [`ProbeFailure`]s reach here; the other two are answers about the bus itself.
    Unaskable(ProbeFailure),
    /// Nothing serves `org.a11y.Bus`, or it owns the name with no bus behind it yet.
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

/// Whether the address's socket file is there. `None` for an address that names no file — an
/// abstract or TCP address is not one whose socket has gone missing.
fn socket_state(address: &str) -> Option<bool> {
    socket_path_from_address(address).map(|p| std::path::Path::new(p).exists())
}

/// Classify the host bus from what the session bus answered, connecting only if it named an
/// address to connect to.
///
/// `connect` and `socket` are thunks: a connection nothing attempted cannot be reported as one
/// that failed.
fn classify_host_bus(
    advertised: Result<Option<String>, ProbeFailure>,
    connect: impl FnOnce() -> Result<(), ProbeFailure>,
    socket: impl FnOnce(&str) -> Option<bool>,
) -> HostBusState {
    let address = match advertised {
        Ok(Some(address)) => address,
        Ok(None) => return HostBusState::NotRunning,
        Err(ProbeFailure::TimedOut(waited)) => return HostBusState::Unresponsive { waited },
        Err(ProbeFailure::Failed(why)) => return HostBusState::Unknown { why },
        Err(nothing_learned @ (ProbeFailure::NotStarted(_) | ProbeFailure::Vanished)) => {
            return HostBusState::Unaskable(nothing_learned);
        }
    };
    let why = match connect() {
        Ok(()) => return HostBusState::Reachable,
        Err(nothing_learned @ (ProbeFailure::NotStarted(_) | ProbeFailure::Vanished)) => {
            return HostBusState::Unaskable(nothing_learned);
        }
        Err(ProbeFailure::Failed(why)) => format!("could not connect to it: {why}"),
        Err(ProbeFailure::TimedOut(waited)) => {
            format!("did not finish connecting to it within {waited:?}")
        }
    };
    HostBusState::Wedged {
        socket: socket(&address),
        address,
        why,
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
        // A launcher glass could not even stat — a permission, not a missing package (glass#474).
        Resolved::Unreadable(p, e) => Check::new(
            "a11y",
            CheckStatus::Warn,
            format!(
                "at-spi-bus-launcher at {} could not be looked at ({e}); it may be installed in \
                 a location glass cannot read",
                p.display()
            ),
        )
        .with_remedy(
            "check that path's permissions, or point GLASS_ATSPI_LAUNCHER at a readable copy",
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
        HostBusState::Wedged { address, socket, why } => Check::new(
            "host desktop a11y",
            CheckStatus::Warn,
            format!(
                "your desktop a11y bus advertises {address} and glass {why}{}",
                match socket {
                    Some(true) => " — its socket file is there",
                    Some(false) => " — nothing exists at that socket path",
                    None => " — its address names no socket file to look for",
                }
            ),
        )
        .with_remedy(RESTART_AT_SPI),
        HostBusState::Unresponsive { waited } => Check::new(
            "host desktop a11y",
            CheckStatus::Warn,
            format!(
                "nothing answered org.a11y.Bus.GetAddress within {waited:?} — the session bus, or \
                 whatever owns that name, is not replying, so host a11y clients block on it"
            ),
        )
        .with_remedy(RESTART_AT_SPI),
        HostBusState::Unknown { why } => Check::new(
            "host desktop a11y",
            CheckStatus::Warn,
            format!("could not tell whether your desktop has an a11y bus: {why}"),
        )
        .with_remedy(ASK_FAILED),
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
    /// The two thunks, for the cases that must never reach them: a verdict that runs either has
    /// classified on something it did not observe.
    fn never_connects() -> Result<(), ProbeFailure> {
        panic!("nothing should connect when no address was answered")
    }

    fn never_stats(_: &str) -> Option<bool> {
        panic!("nothing should look for a socket when no address was answered")
    }

    fn refused() -> Result<(), ProbeFailure> {
        Err(ProbeFailure::Failed("Connection refused".into()))
    }

    fn address() -> Result<Option<String>, ProbeFailure> {
        Ok(Some("unix:path=/x".into()))
    }

    fn wedged(socket: Option<bool>, why: &str) -> HostBusState {
        HostBusState::Wedged {
            address: "unix:path=/x".into(),
            socket,
            why: why.into(),
        }
    }

    #[test]
    fn a_bus_glass_connected_to_is_reachable() {
        assert_eq!(
            classify_host_bus(address(), || Ok(()), never_stats),
            HostBusState::Reachable
        );
    }

    #[test]
    fn an_advertised_bus_that_will_not_connect_is_wedged() {
        assert_eq!(
            classify_host_bus(address(), refused, |_| Some(false)),
            wedged(Some(false), "could not connect to it: Connection refused")
        );
    }

    /// A socket that is still there does not make an unconnectable bus a host without one — it
    /// fell to the green arm (glass#455).
    #[test]
    fn an_advertised_bus_is_still_wedged_when_its_socket_is_there() {
        assert_eq!(
            classify_host_bus(address(), refused, |_| Some(true)),
            wedged(Some(true), "could not connect to it: Connection refused")
        );
    }

    /// An abstract or TCP address names no file, so "its socket is gone" would report a finding
    /// nothing looked for.
    #[test]
    fn an_address_naming_no_file_is_not_a_socket_that_went_missing() {
        assert_eq!(
            classify_host_bus(address(), refused, |_| None),
            wedged(None, "could not connect to it: Connection refused")
        );
    }

    /// A connection that ran out of time is not one the bus refused.
    #[test]
    fn a_connection_that_timed_out_says_so_rather_than_claiming_a_refusal() {
        let state = classify_host_bus(
            address(),
            || Err(ProbeFailure::TimedOut(Duration::from_millis(1500))),
            |_| Some(true),
        );
        assert_eq!(
            state,
            wedged(Some(true), "did not finish connecting to it within 1.5s")
        );
    }

    /// The one this check exists for: a bus that takes `GetAddress` and never answers used to
    /// land in the arm that says nothing is wrong (glass#455).
    #[test]
    fn a_bus_that_never_answered_is_not_a_host_without_one() {
        let waited = Duration::from_millis(1500);
        assert_eq!(
            classify_host_bus(
                Err(ProbeFailure::TimedOut(waited)),
                never_connects,
                never_stats
            ),
            HostBusState::Unresponsive { waited }
        );
    }

    /// The launcher answers `""` until it has a bus to name, and calling that a wedge would print
    /// a detail naming an empty address.
    #[test]
    fn no_address_at_all_is_a_host_without_a_bus() {
        assert_eq!(
            classify_host_bus(Ok(None), never_connects, never_stats),
            HostBusState::NotRunning
        );
    }

    /// A question the session bus answered with an error is not the answer "there is no bus" —
    /// an at-spi launcher that fails to start lands here, and used to read green (glass#455).
    #[test]
    fn a_question_that_failed_is_not_an_answer_that_there_is_no_bus() {
        let state = classify_host_bus(
            Err(ProbeFailure::Failed("Spawn.ChildExited".into())),
            never_connects,
            never_stats,
        );
        assert_eq!(
            state,
            HostBusState::Unknown {
                why: "Spawn.ChildExited".into()
            }
        );
    }

    /// A probe the host refused learned nothing about the bus.
    #[test]
    fn a_probe_that_never_ran_is_not_a_verdict_on_the_host() {
        let refused = ProbeFailure::NotStarted("Resource temporarily unavailable".into());
        assert_eq!(
            classify_host_bus(Err(refused.clone()), never_connects, never_stats),
            HostBusState::Unaskable(refused)
        );
    }

    #[test]
    fn a_getaddress_worker_that_unwound_is_not_a_host_without_a_bus() {
        assert_eq!(
            classify_host_bus(Err(ProbeFailure::Vanished), never_connects, never_stats),
            HostBusState::Unaskable(ProbeFailure::Vanished)
        );
    }

    /// The same for the second probe: a connect thread that unwound is not a bus that refused.
    #[test]
    fn a_connect_worker_that_unwound_is_not_a_bus_that_refused() {
        assert_eq!(
            classify_host_bus(address(), || Err(ProbeFailure::Vanished), never_stats),
            HostBusState::Unaskable(ProbeFailure::Vanished)
        );
    }

    /// The socket check reads the address that was answered, not some other one.
    #[test]
    fn the_socket_is_looked_for_at_the_address_that_was_answered() {
        let state = classify_host_bus(address(), refused, |addr| {
            assert_eq!(addr, "unix:path=/x");
            Some(true)
        });
        assert_eq!(
            state,
            wedged(Some(true), "could not connect to it: Connection refused")
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

    /// The unusable states point at the host's at-spi — a remedy about glass's own probe would
    /// be the wrong half of the diagnosis.
    #[test]
    fn the_states_that_found_an_unusable_bus_prescribe_the_restart() {
        for bus in [
            wedged(Some(false), "could not connect to it: Connection refused"),
            HostBusState::Unresponsive {
                waited: Duration::from_millis(1500),
            },
        ] {
            let h = reported(bus);
            assert_eq!(h.status, CheckStatus::Warn, "{h:#?}");
            assert!(
                h.remedy
                    .clone()
                    .unwrap()
                    .contains("restart the host's at-spi"),
                "{h:#?}"
            );
        }
    }

    /// Three answers, not two: an address that names no file is not a socket that went missing.
    #[test]
    fn a_wedged_bus_says_what_was_found_about_its_socket() {
        let details = [Some(false), Some(true), None]
            .map(|socket| reported(wedged(socket, "could not connect to it: x")).detail);
        let unique: std::collections::BTreeSet<&String> = details.iter().collect();
        assert_eq!(unique.len(), 3, "{details:#?}");
        assert!(
            details.iter().all(|d| d.contains("unix:path=/x")),
            "{details:#?}"
        );
    }

    /// The host failure the check was added to catch, and the one it reported in green.
    #[test]
    fn a_hung_host_bus_warns_and_says_how_long_it_waited() {
        let h = reported(HostBusState::Unresponsive {
            waited: Duration::from_millis(1500),
        });
        assert_eq!(h.status, CheckStatus::Warn);
        assert!(h.detail.contains("1.5s"), "{}", h.detail);
        assert!(h.remedy.is_some());
    }

    /// A host with no a11y bus is the ordinary headless case, and warning at it would make every
    /// container's doctor run noise.
    #[test]
    fn a_host_that_has_no_a11y_bus_still_reads_green() {
        let h = reported(HostBusState::NotRunning);
        assert_eq!(h.status, CheckStatus::Ok);
        assert!(h.remedy.is_none(), "{h:#?}");
    }

    /// A question the session bus refused is not an answer about the host, so its remedy points
    /// at the error rather than at the operator's at-spi processes.
    #[test]
    fn a_question_that_failed_reports_the_error_it_got() {
        let h = reported(HostBusState::Unknown {
            why: "org.freedesktop.DBus.Error.Spawn.ChildExited".into(),
        });
        assert_eq!(h.status, CheckStatus::Warn);
        assert!(h.detail.contains("Spawn.ChildExited"), "{}", h.detail);
        assert!(
            !h.remedy
                .clone()
                .unwrap()
                .contains("restart the host's at-spi"),
            "{:?}",
            h.remedy
        );
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
        assert!(!remedy.contains("restart the host's at-spi"), "{remedy}");
        // Sending an operator to check `/tmp` over an `EAGAIN` is the same misdirection one
        // layer down.
        assert!(!remedy.contains("temp dir"), "{remedy}");
    }

    /// Each state is a different thing to do about it, so no two may print the same line.
    #[test]
    fn every_host_bus_state_says_something_different() {
        let details = [
            HostBusState::Reachable,
            wedged(Some(false), "could not connect to it: x"),
            wedged(Some(true), "could not connect to it: x"),
            HostBusState::Unresponsive {
                waited: Duration::from_millis(1500),
            },
            HostBusState::Unknown { why: "x".into() },
            HostBusState::Unaskable(ProbeFailure::Vanished),
            HostBusState::NotRunning,
        ]
        .map(|bus| reported(bus).detail);
        let unique: std::collections::BTreeSet<&String> = details.iter().collect();
        assert_eq!(unique.len(), details.len(), "{details:#?}");
    }

    // ---- the probe seam both host-bus probes run through ----
    /// A call the bus answered with an error is the bus's answer, not a wait that ran out — the
    /// distinction a single `Err(_)` arm loses.
    #[test]
    fn a_probe_that_failed_is_not_reported_as_one_that_timed_out() {
        let out: Result<(), ProbeFailure> =
            probe_on_a_private_runtime(Duration::from_secs(30), || async {
                Err::<(), String>("bus said no".into())
            });
        assert_eq!(out, Err(ProbeFailure::Failed("bus said no".into())));
    }

    /// A worker that unwound arrives at once, and is not a wait that elapsed. The panic below is
    /// deliberate — its message on stderr is what the remedy tells the operator to look for.
    #[test]
    fn a_probe_whose_thread_unwound_is_not_reported_as_a_timeout() {
        let started = std::time::Instant::now();
        let out: Result<(), ProbeFailure> =
            probe_on_a_private_runtime(Duration::from_secs(30), || async {
                panic!("this probe panics on purpose (glass#455)")
            });
        assert_eq!(out, Err(ProbeFailure::Vanished));
        // Reporting it after the budget would claim a wait nobody did.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
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
