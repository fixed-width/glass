//! Environment checks for the X11 backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`x11_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without a display.

use std::sync::mpsc;
use std::time::Duration;

use glass_core::{Check, CheckStatus, ProbeFailure};
use glass_exec_unix::{Resolved, resolve_bin};
use x11rb::connect;
use x11rb::errors::ConnectError;

use crate::xvfb::Xvfb;

/// Probe the X11 backend's environment. `deep` additionally spawns and tears down a
/// private Xvfb (when in self-spawn mode) to prove it actually starts.
pub fn checks(deep: bool) -> Vec<Check> {
    checks_for(std::env::var("GLASS_DISPLAY").ok().as_deref(), deep)
}

/// The gathering layer, with the one value that selects between attach and self-spawn mode
/// passed in — mutating `GLASS_DISPLAY` to reach the other mode is `unsafe` under edition
/// 2024, and process-global besides.
fn checks_for(glass_display: Option<&str>, deep: bool) -> Vec<Check> {
    match glass_display.map(str::trim).filter(|s| !s.is_empty()) {
        Some(display) => x11_checks(&Mode::Attach {
            display,
            verdict: attach_verdict(&crate::platform::normalize_display(display)),
        }),
        None => {
            let xvfb = resolve_bin(
                &glass_core::tool_path("GLASS_XVFB", "Xvfb"),
                std::env::var_os("PATH").as_deref(),
            );
            x11_checks(&Mode::SelfSpawn {
                xvfb: &xvfb,
                deep: deep.then(probe_xvfb),
            })
        }
    }
}

/// What was gathered, in the shape the mapper reads. The two modes cannot be crossed: an attach
/// has no Xvfb to find, a self-spawn no display to reach. Passed as separate arguments, "probed
/// and unreachable" and "never probed" were one wildcard arm apart (glass#373).
enum Mode<'a> {
    /// `GLASS_DISPLAY` names a display; the only question is whether glass can attach to it.
    Attach {
        display: &'a str,
        verdict: AttachVerdict,
    },
    /// No `GLASS_DISPLAY`: glass starts its own Xvfb, which must be present and — on a deep
    /// check — provably startable.
    SelfSpawn {
        xvfb: &'a Resolved,
        deep: Option<Result<String, ProbeFailure>>,
    },
}

/// Pure: build the X11 checks from gathered facts.
fn x11_checks(mode: &Mode) -> Vec<Check> {
    match mode {
        Mode::SelfSpawn { xvfb, deep } => {
            let mut checks = vec![
                Check::new(
                    "GLASS_DISPLAY",
                    CheckStatus::Ok,
                    "unset — glass will spawn a private headless Xvfb",
                ),
                xvfb_check(xvfb),
            ];
            checks.extend(deep.as_ref().map(deep_spawn_check));
            checks
        }
        Mode::Attach { display, verdict } => vec![match verdict {
            AttachVerdict::Answered(Ok(())) => Check::new(
                "GLASS_DISPLAY",
                CheckStatus::Ok,
                format!("{display} — reachable; glass will attach to it"),
            ),
            AttachVerdict::Answered(Err(no)) => Check::new(
                "GLASS_DISPLAY",
                CheckStatus::Fail,
                format!("{display} — {}", no.cause),
            )
            .with_remedy(no.remedy),
            // A server that accepts and goes silent is not "unreachable" — nothing to start — and
            // attaching would hang, so the check fails rather than passes (glass#462).
            AttachVerdict::TimedOut(budget) => Check::new(
                "GLASS_DISPLAY",
                CheckStatus::Fail,
                format!("{display} — the server accepted the connection but did not answer within {budget:?}"),
            )
            .with_remedy(ATTACH_WEDGED),
        }],
    }
}

/// The Xvfb glass would spawn: is there one, and can this process run it?
fn xvfb_check(xvfb: &Resolved) -> Check {
    match xvfb {
        Resolved::Found(p) => Check::new("Xvfb", CheckStatus::Ok, p.display().to_string()),
        Resolved::NotExecutable(p) => Check::new(
            "Xvfb",
            CheckStatus::Fail,
            format!("{} — not executable", p.display()),
        )
        .with_remedy(format!(
            "chmod +x {}, or point GLASS_XVFB at a runnable binary",
            p.display()
        )),
        // Xvfb may be present, but glass could not stat it: a permission, not an install
        // (glass#474).
        Resolved::Unreadable(p, e) => Check::new(
            "Xvfb",
            CheckStatus::Fail,
            format!(
                "{} — could not be looked at ({e}); it may be installed where glass cannot read it",
                p.display()
            ),
        )
        .with_remedy("check that path's permissions, or point GLASS_XVFB at a readable copy"),
        Resolved::Absent => Check::new("Xvfb", CheckStatus::Fail, "not found").with_remedy(
            "install it (e.g. `apt install xvfb`), set GLASS_XVFB to its path, or set \
             GLASS_DISPLAY=:N to attach to an existing display",
        ),
        // Nothing was searched, so nothing is known about whether Xvfb is installed. MCP clients
        // routinely spawn glass-mcp with a stripped environment (glass#373).
        Resolved::NoSearchPath => Check::new(
            "Xvfb",
            CheckStatus::Fail,
            "could not be looked up — PATH is unset in glass's environment",
        )
        .with_remedy(
            "set GLASS_XVFB to Xvfb's absolute path, give glass a PATH to search, or set \
             GLASS_DISPLAY=:N to attach to an existing display",
        ),
    }
}

/// What a deep probe proved. Its remedy comes from [`ProbeFailure`], which withholds
/// [`XVFB_START_HINT`] from the outcomes that never reached Xvfb.
fn deep_spawn_check(spawn: &Result<String, ProbeFailure>) -> Check {
    match spawn {
        Ok(display) => Check::new(
            "Xvfb spawn (deep)",
            CheckStatus::Ok,
            format!("started and stopped {display}"),
        ),
        Err(failure) => Check::new(
            "Xvfb spawn (deep)",
            CheckStatus::Fail,
            failure.detail("Xvfb"),
        )
        .with_remedy(failure.remedy(XVFB_START_HINT)),
    }
}

/// What to check when Xvfb itself is what failed. It does not claim Xvfb is installed: the detail
/// is `Xvfb::start`'s own message, which for a spawn failure says the opposite.
const XVFB_START_HINT: &str = "the detail is Xvfb's own answer — check its dependencies and \
     permissions, or set GLASS_DISPLAY=:N to attach to an existing display instead";

/// Why glass could not attach to the display `GLASS_DISPLAY` names.
///
/// Cause and remedy travel together because the pairing is the point: a server that answered and
/// refused glass is running, and the advice to start one sends the operator to start a second
/// (glass#373).
#[derive(Debug, Clone, PartialEq, Eq)]
struct NoAttach {
    cause: String,
    remedy: &'static str,
}

const START_THE_DISPLAY: &str = "start that display (e.g. `./scripts/sandbox-xvfb.sh start` for \
     :42) or unset GLASS_DISPLAY to self-spawn";
const FIX_THE_AUTHORITY: &str = "set XAUTHORITY to that display's cookie file — for a server \
     spawned by an MCP client, in the client's `env` block, since glass inherits its environment \
     (`xauth list` shows what a cookie file holds) — or, from a session that already reaches the \
     display, run `xhost +si:localuser:$USER`";
const SERVER_ANSWERED: &str = "the server answered and the connection still failed — its own \
     reason is in the detail; unset GLASS_DISPLAY to self-spawn instead";
// `normalize_display` prepends `:` to anything without one, so `host:0` becomes `:host:0` and X
// then reads `:host` as the hostname: a remote display cannot be named here at all.
const NAME_A_DISPLAY: &str = "GLASS_DISPLAY names a local display — `:42`, bare `42`, or `:42.0` \
     to pick a screen; a remote `host:0` is not supported";
const PICK_A_SCREEN: &str = "name a screen that server has — the `.N` suffix, e.g. `:42.0` (most \
     servers have only screen 0) — or unset GLASS_DISPLAY to self-spawn";

/// What the attach probe learned about the display `GLASS_DISPLAY` names. The connect used to
/// be unbounded (glass#462): a server that accepted and went silent blocked the doctor with no
/// check and no way to say which probe was stuck. `Answered` is the real handshake outcome;
/// `TimedOut` is the server that accepts and goes silent.
#[derive(Debug)]
enum AttachVerdict {
    /// `x11rb::connect` returned a connection or a refusal.
    Answered(Result<(), NoAttach>),
    /// The server accepted (or the connect hung elsewhere) and had not answered within the
    /// budget. Carries the budget it waited.
    TimedOut(Duration),
}

/// How long the attach probe waits: a one-shot like the other doctor probes, and 10s matches
/// the connect timeouts elsewhere in glass (idb, the release check).
const ATTACH_PROBE_BUDGET: Duration = Duration::from_secs(10);

/// The remedy for a server that accepted and went silent: there is nothing to start, so inspect
/// it rather than start it. The budget the check waited is in the detail.
const ATTACH_WEDGED: &str = "a server that accepts and goes silent is not one to start — \
     check the display with `xdpyinfo` or `xhost` by hand, and see its logs";

/// Bounded `x11rb::connect`, with the outcomes kept apart the way `probe_xvfb` keeps them:
/// a refusal is an answer, a hang is not. The connect runs on its own thread because there is
/// no deadline inside `x11rb::connect` to interrupt — the join must not wait on the server.
fn attach_verdict(display: &str) -> AttachVerdict {
    let display = display.to_string();
    let (tx, rx) = mpsc::channel();
    let spawn = std::thread::Builder::new()
        .name("glass-doctor-x11-attach".into())
        .spawn(move || {
            let _ = tx.send(
                connect(Some(display.as_str()))
                    .map(|_| ())
                    .map_err(|e| no_attach(&e)),
            );
        });
    match spawn {
        // The thread runs; wait on it the way the other two outcomes are waited on.
        Ok(_handle) => await_verdict(&rx),
        // The host refused the probe's own thread (a low `pids` cgroup limit is the usual
        // cause): there was no connect to hang, so it is an answer, not a timeout.
        Err(e) => AttachVerdict::Answered(Err(NoAttach {
            cause: format!("the host refused the probe's own thread, so nothing was probed: {e}"),
            remedy: "a low `pids` cgroup limit is the usual cause; free up threads on the host, \
                 or unset GLASS_DISPLAY to self-spawn instead",
        })),
    }
}

/// The bounded wait on the connect. A sender that dropped unsent is a probe thread that
/// unwound, and arrives immediately: called a timeout it claims a wait nobody did (glass#373).
fn await_verdict(rx: &mpsc::Receiver<Result<(), NoAttach>>) -> AttachVerdict {
    match rx.recv_timeout(ATTACH_PROBE_BUDGET) {
        Ok(v) => AttachVerdict::Answered(v),
        // `Disconnected` is not a hang: the thread unwound, and the wait it never did must not be
        // reported as one. The panic is on glass's stderr.
        Err(mpsc::RecvTimeoutError::Disconnected) => AttachVerdict::Answered(Err(NoAttach {
            cause: "the attach probe ended without an answer — its thread unwound".into(),
            remedy: "the panic is on glass's stderr; a `pids` cgroup limit low enough to refuse \
                 a thread will also do this",
        })),
        Err(mpsc::RecvTimeoutError::Timeout) => AttachVerdict::TimedOut(ATTACH_PROBE_BUDGET),
    }
}

/// Classify a connect failure. What falls to the wildcard is `IoError` — nothing was listening,
/// the case the advice fits — plus the local failures (`ParseError`, `InsufficientMemory`,
/// `UnknownError`) and whatever this `#[non_exhaustive]` enum gains next.
fn no_attach(e: &ConnectError) -> NoAttach {
    match e {
        // The server answered and turned glass away, so it is running and a second one cannot
        // help. Status 2 is an authentication demand outright; status 0 is the generic refusal,
        // carrying "Maximum number of clients reached" as readily as an authorisation message, so
        // its own reason picks the remedy.
        ConnectError::SetupAuthenticate(_) => NoAttach {
            cause: format!("the server refused the connection ({e})"),
            remedy: FIX_THE_AUTHORITY,
        },
        ConnectError::SetupFailed(f) => NoAttach {
            cause: format!("the server refused the connection ({e})"),
            remedy: if refused_over_authority(&f.reason) {
                FIX_THE_AUTHORITY
            } else {
                SERVER_ANSWERED
            },
        },
        // The handshake began and did not finish, so something answered: the same reason
        // "start that display" is wrong for a refusal.
        ConnectError::Incomplete { .. } | ConnectError::ZeroIdMask => NoAttach {
            cause: format!("the handshake did not complete ({e})"),
            remedy: SERVER_ANSWERED,
        },
        ConnectError::DisplayParsingError(_) => NoAttach {
            cause: format!("not a display name X can parse ({e})"),
            remedy: NAME_A_DISPLAY,
        },
        ConnectError::InvalidScreen => NoAttach {
            cause: "the server is running and has no such screen".into(),
            remedy: PICK_A_SCREEN,
        },
        _ => NoAttach {
            cause: format!("cannot connect ({e})"),
            remedy: START_THE_DISPLAY,
        },
    }
}

/// Whether a refusal is about authorisation, read from the server's own reason text — X sends the
/// same status byte for every refusal, so the variant alone cannot say.
///
/// Two terms, because X.org's refusals are worded two ways: "Authorization required, but no
/// authorization protocol specified" and "Client is not authorized to connect to Server" carry the
/// first, "Invalid MIT-MAGIC-COOKIE-1 key" only the second.
fn refused_over_authority(reason: &[u8]) -> bool {
    let reason = String::from_utf8_lossy(reason).to_lowercase();
    reason.contains("authoriz") || reason.contains("cookie")
}

/// Margin over `Xvfb::start`'s own worst case, so the backstop effectively never fires and
/// the probe thread always finishes and reaps its child.
const PROBE_MARGIN: Duration = Duration::from_secs(2);

/// How long the deep probe waits. Must exceed `Xvfb::start`'s OWN worst case, which
/// includes one retry of a wedged server: a shorter budget reports Fail for the exact
/// transient class the start path survives, with a wrong remedy.
fn probe_budget() -> Duration {
    crate::xvfb::start_deadline() + PROBE_MARGIN
}

/// Spawn a private Xvfb and tear it down, with a timeout so a wedged Xvfb can't hang
/// doctor. Returns the display it came up on, or why there was none.
fn probe_xvfb() -> Result<String, ProbeFailure> {
    let screen = std::env::var("GLASS_XVFB_SCREEN").unwrap_or_else(|_| "1280x800x24".into());
    let budget = probe_budget();
    let (tx, rx) = mpsc::channel();
    // Fallible, unlike a bare `spawn`, which panics when the OS refuses a thread — in doctor that
    // is an unwind where a check belongs. `Xvfb::start` spawns two more threads with bare `spawn`,
    // and those unwind inside this one, arriving as `Vanished`.
    std::thread::Builder::new()
        .name("glass-doctor-xvfb".into())
        .spawn(move || {
            // The Xvfb is dropped at the end of `map` (after we read its display),
            // tearing the test display back down.
            let _ = tx.send(
                Xvfb::start(&screen)
                    .map(|x| x.display.clone())
                    .map_err(|e| e.to_string()),
            );
        })
        .map_err(|e| ProbeFailure::NotStarted(e.to_string()))?;
    await_probe(&rx, budget)
}

/// The bounded wait, kept apart from the spawn so a test can drive every outcome without an X
/// server. A sender that dropped unsent is a probe thread that unwound, and arrives immediately:
/// called a timeout it claims a wait nobody did (glass#373).
fn await_probe(
    rx: &mpsc::Receiver<Result<String, String>>,
    budget: Duration,
) -> Result<String, ProbeFailure> {
    match rx.recv_timeout(budget) {
        Ok(Ok(display)) => Ok(display),
        Ok(Err(e)) => Err(ProbeFailure::Failed(e)),
        Err(e) => Err(ProbeFailure::from_recv(e, budget)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A shallow self-spawn check: the mode glass runs in with no `GLASS_DISPLAY` set.
    fn self_spawn(xvfb: &Resolved) -> Vec<Check> {
        x11_checks(&Mode::SelfSpawn { xvfb, deep: None })
    }

    fn found() -> Resolved {
        Resolved::Found(PathBuf::from("/usr/bin/Xvfb"))
    }

    #[test]
    fn self_spawn_with_xvfb_present_is_ok() {
        let cs = self_spawn(&found());
        assert_eq!(cs[0].name, "GLASS_DISPLAY");
        assert_eq!(cs[0].status, CheckStatus::Ok);
        assert_eq!(cs[1].name, "Xvfb");
        assert_eq!(cs[1].status, CheckStatus::Ok);
        assert_eq!(cs[1].detail, "/usr/bin/Xvfb");
    }

    #[test]
    fn self_spawn_without_xvfb_fails_with_remedy() {
        let cs = self_spawn(&Resolved::Absent);
        let xvfb = cs.iter().find(|c| c.name == "Xvfb").unwrap();
        assert_eq!(xvfb.status, CheckStatus::Fail);
        assert!(xvfb.remedy.as_deref().unwrap().contains("apt install xvfb"));
    }

    #[test]
    fn attach_reachable_is_ok_unreachable_fails() {
        let ok = x11_checks(&Mode::Attach {
            display: ":42",
            verdict: AttachVerdict::Answered(Ok(())),
        });
        assert_eq!(ok[0].status, CheckStatus::Ok);
        assert!(
            ok.iter().all(|c| c.name != "Xvfb"),
            "attach mode shouldn't require Xvfb"
        );

        let bad = x11_checks(&Mode::Attach {
            display: ":42",
            verdict: AttachVerdict::Answered(Err(no_attach(&ConnectError::UnknownError))),
        });
        assert_eq!(bad[0].status, CheckStatus::Fail);
        assert!(bad[0].remedy.is_some());
    }

    /// The reason is the half that tells two failures apart, and doctor's detail is the only
    /// place it is printed.
    #[test]
    fn an_unreachable_display_carries_the_reason_into_the_detail() {
        let cs = x11_checks(&Mode::Attach {
            display: ":42",
            verdict: AttachVerdict::Answered(Err(no_attach(&refusal("Client is not authorized")))),
        });
        assert!(cs[0].detail.contains(":42"), "{:?}", cs[0].detail);
        assert!(
            cs[0].detail.contains("Client is not authorized"),
            "{:?}",
            cs[0].detail
        );
    }

    /// The regression glass#462 pins: a server that accepts and goes silent is reported as a
    /// hang — not "unreachable" (nothing to start) and not a pass (attaching would hang) — and
    /// the remedy is the wedged one, not the start-the-display one.
    #[test]
    fn a_wedged_server_fails_the_check_and_gets_the_wedged_remedy() {
        let cs = x11_checks(&Mode::Attach {
            display: ":42",
            verdict: AttachVerdict::TimedOut(ATTACH_PROBE_BUDGET),
        });
        assert_eq!(cs[0].status, CheckStatus::Fail);
        assert!(cs[0].detail.contains("accepted"), "{:?}", cs[0].detail);
        assert!(cs[0].detail.contains("10s"), "{:?}", cs[0].detail);
        assert_eq!(
            cs[0].remedy.as_deref(),
            Some(ATTACH_WEDGED),
            "a wedged server must not be told to start a display: {:?}",
            cs[0].remedy
        );
        assert!(!ATTACH_WEDGED.contains(START_THE_DISPLAY), "sanity");
    }

    /// The wait passes through every connect answer it can be given: a refusal, a success, a
    /// hang, and a probe thread that unwound. The last arrives at once; calling it a hang would
    /// claim a wait nobody did (glass#373).
    #[test]
    fn the_attach_wait_keeps_a_refusal_a_hang_and_an_unwound_probe_apart() {
        let (tx, rx) = mpsc::channel::<Result<(), NoAttach>>();
        tx.send(Ok(())).expect("send");
        assert!(matches!(
            await_verdict(&rx),
            AttachVerdict::Answered(Ok(()))
        ));

        let (tx, rx) = mpsc::channel::<Result<(), NoAttach>>();
        tx.send(Err(no_attach(&ConnectError::ZeroIdMask)))
            .expect("send");
        assert!(matches!(
            await_verdict(&rx),
            AttachVerdict::Answered(Err(_))
        ));

        let (tx, rx) = mpsc::channel::<Result<(), NoAttach>>();
        let started = std::time::Instant::now();
        drop(tx);
        assert!(matches!(
            await_verdict(&rx),
            AttachVerdict::Answered(Err(no)) if no.cause.contains("unwound")
        ));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an unwound probe arrives at once: {:?}",
            started.elapsed()
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_live_display_is_reachable_and_an_unused_number_is_not() {
        let server = Xvfb::start("640x480x24").expect("Xvfb should start");
        assert!(
            matches!(
                attach_verdict(&server.display),
                AttachVerdict::Answered(Ok(()))
            ),
            "the display we just started must be reachable"
        );
        drop(server);
        let no = match attach_verdict(":9999") {
            AttachVerdict::Answered(Err(no)) => no,
            other => panic!("an unused display number is not reachable: {other:?}"),
        };
        assert_eq!(
            no.remedy, START_THE_DISPLAY,
            "nothing is listening, so starting one is the fix: {no:?}"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn the_deep_probe_starts_a_real_display_and_takes_it_back_down() {
        let display = probe_xvfb().expect("the deep probe should start a display");
        // Only the report is asserted. Whether the number still answers is not this test's
        // to check: display numbers are a machine-wide namespace, and the next server to
        // start — in this suite or outside it — takes the one the probe just released.
        assert!(
            display.starts_with(':'),
            "must report the display it came up on, got {display:?}"
        );
    }

    #[test]
    fn the_probe_budget_outlasts_the_start_it_wraps() {
        assert!(
            probe_budget() > crate::xvfb::start_deadline(),
            "probe budget {:?} must exceed the start deadline {:?}",
            probe_budget(),
            crate::xvfb::start_deadline()
        );
    }

    #[test]
    fn a_blank_glass_display_is_treated_as_unset() {
        // Blank means "self-spawn", so it must not be carried into attach mode and
        // reported as an unreachable display named "".
        let cs = checks_for(Some("   "), false);
        assert!(
            cs.iter().any(|c| c.name == "Xvfb"),
            "blank should select self-spawn mode, which checks for Xvfb: {cs:?}"
        );
    }

    #[test]
    fn a_named_glass_display_selects_attach_mode() {
        let cs = checks_for(Some(":9999"), false);
        assert!(
            cs.iter().all(|c| c.name != "Xvfb"),
            "attach mode must not require Xvfb: {cs:?}"
        );
        assert_eq!(cs[0].status, CheckStatus::Fail, "{cs:?}");
    }

    #[test]
    fn a_shallow_check_never_spawns_a_display() {
        // The deep probe costs a real Xvfb start; running it on a shallow check makes
        // `glass doctor` spawn a server nobody asked for.
        let cs = checks_for(None, false);
        assert!(
            cs.iter().all(|c| c.name != "Xvfb spawn (deep)"),
            "shallow mode must not run the deep probe: {cs:?}"
        );
    }

    #[test]
    fn the_public_entry_point_reports_the_display_mode() {
        let cs = checks(false);
        assert!(
            cs.iter().any(|c| c.name == "GLASS_DISPLAY"),
            "doctor must always say which display mode it is in: {cs:?}"
        );
    }

    /// glass#374: `GLASS_XVFB` pointing at a file without the execute bit used to report `Ok`,
    /// and `Xvfb::start` then failed with EACCES — the one outcome doctor exists to prevent.
    /// "not found" would be wrong too: the file is there, so installing xvfb again cannot help.
    #[test]
    fn a_non_executable_xvfb_fails_with_a_chmod_remedy() {
        let p = PathBuf::from("/opt/x/Xvfb");
        let cs = self_spawn(&Resolved::NotExecutable(p));
        let xvfb = cs.iter().find(|c| c.name == "Xvfb").expect("an Xvfb check");
        assert_eq!(xvfb.status, CheckStatus::Fail);
        assert!(
            xvfb.detail.contains("/opt/x/Xvfb") && xvfb.detail.contains("not executable"),
            "must name the file and say why: {:?}",
            xvfb.detail
        );
        assert!(
            xvfb.remedy
                .as_deref()
                .is_some_and(|r| r.contains("chmod +x")),
            "the remedy is chmod, not a reinstall: {:?}",
            xvfb.remedy
        );
    }

    fn refusal(reason: &str) -> ConnectError {
        ConnectError::SetupFailed(x11rb::protocol::xproto::SetupFailed {
            reason: reason.as_bytes().to_vec(),
            ..Default::default()
        })
    }

    /// glass#373: the classic X11 attach failure is authorisation — `XAUTHORITY` unset or holding
    /// the wrong cookie. Told to start the display, the operator starts a second one and nothing
    /// anywhere names auth.
    #[test]
    fn a_display_that_refused_the_connection_is_not_one_that_needs_starting() {
        for e in [
            refusal("Client is not authorized to connect to Server"),
            ConnectError::SetupAuthenticate(x11rb::protocol::xproto::SetupAuthenticate {
                reason: b"no cookie".to_vec(),
                ..Default::default()
            }),
        ] {
            let no = no_attach(&e);
            assert!(
                no.cause.contains("refused"),
                "the server answered, and the detail must say so: {no:?}"
            );
            assert!(
                no.remedy.contains("XAUTHORITY"),
                "an auth failure wants the cookie, not a second server: {no:?}"
            );
            assert!(
                !no.remedy.contains("start that display"),
                "the display is already running: {no:?}"
            );
        }
    }

    /// A rejected cookie is worded without the word "authorization", so matching that alone would
    /// send the operator with a stale cookie to "start that display".
    #[test]
    fn a_rejected_cookie_is_an_authorisation_failure_too() {
        let no = no_attach(&refusal("Invalid MIT-MAGIC-COOKIE-1 key"));
        assert!(no.remedy.contains("XAUTHORITY"), "{no:?}");
    }

    /// The refusal reason is the server's own, and the only text that says which of the auth
    /// failures this is.
    #[test]
    fn a_refusal_carries_the_servers_reason() {
        let no = no_attach(&refusal(
            "Authorization required, but no authorization protocol \
                                     specified",
        ));
        assert!(
            no.cause.contains("Authorization required"),
            "{:?}",
            no.cause
        );
    }

    /// Setup status 0 is the server's generic "refused", not an auth verdict: a server at its
    /// client limit sends it too, and telling that operator to fix a cookie is the misdirection
    /// this check exists to remove. The reason is the server's own, so it decides.
    #[test]
    fn a_refusal_that_is_not_about_authorisation_does_not_get_the_cookie_remedy() {
        let no = no_attach(&refusal("Maximum number of clients reached"));
        assert!(no.cause.contains("Maximum number of clients"), "{no:?}");
        assert!(
            !no.remedy.contains("XAUTHORITY"),
            "nothing here is about a cookie: {no:?}"
        );
        assert!(
            !no.remedy.contains("start that display"),
            "the server answered, so it is running: {no:?}"
        );
    }

    /// A handshake that began and did not finish: the server answered, so "start that display" is
    /// wrong for the same reason it is wrong for a refusal.
    #[test]
    fn a_handshake_that_broke_off_is_not_a_display_that_needs_starting() {
        for e in [
            ConnectError::Incomplete {
                expected: 8,
                received: 2,
            },
            ConnectError::ZeroIdMask,
        ] {
            let no = no_attach(&e);
            assert!(!no.remedy.contains("start that display"), "{no:?}");
        }
    }

    #[test]
    fn nothing_listening_on_the_display_is_still_told_to_start_it() {
        let no = no_attach(&ConnectError::IoError(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        )));
        assert!(no.remedy.contains("start that display"), "{no:?}");
    }

    /// `GLASS_DISPLAY=localhost` is not a display name, and no display anyone starts will make it
    /// one.
    #[test]
    fn a_display_name_x_cannot_parse_says_that_rather_than_cannot_connect() {
        let no = no_attach(&ConnectError::DisplayParsingError(
            x11rb::errors::DisplayParsingError::MalformedValue("localhost".into()),
        ));
        assert!(no.remedy.contains("GLASS_DISPLAY"), "{no:?}");
        assert!(!no.remedy.contains("start that display"), "{no:?}");
    }

    /// A running server reached on a screen it does not have. Starting another one gives the same
    /// answer.
    #[test]
    fn a_server_without_that_screen_is_not_a_server_that_is_down() {
        let no = no_attach(&ConnectError::InvalidScreen);
        assert!(no.cause.contains("screen"), "{no:?}");
        assert!(!no.remedy.contains("start that display"), "{no:?}");
    }

    fn deep(spawn: Result<String, ProbeFailure>) -> Check {
        x11_checks(&Mode::SelfSpawn {
            xvfb: &found(),
            deep: Some(spawn),
        })
        .into_iter()
        .find(|c| c.name == "Xvfb spawn (deep)")
        .expect("a deep probe contributes its check")
    }

    /// The arm every successful deep check renders, and the only one no failure test reaches:
    /// dropped or restyled, `glass doctor --deep` would report a spawn it never proved.
    #[test]
    fn a_deep_probe_that_worked_names_the_display_it_started() {
        let check = deep(Ok(":42".into()));
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.detail.contains(":42"), "{:?}", check.detail);
        assert!(check.remedy.is_none(), "nothing to fix: {check:?}");
    }

    #[test]
    fn deep_spawn_failure_is_reported() {
        let check = deep(Err(ProbeFailure::Failed("boom".into())));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("boom"), "{:?}", check.detail);
        assert!(
            check.remedy.as_deref().is_some_and(|r| r.contains("Xvfb")),
            "a probe that reached Xvfb gets Xvfb's advice: {:?}",
            check.remedy
        );
    }

    /// glass#373: `std::thread::spawn` panics on `EAGAIN` and `Xvfb::start` spawns two threads of
    /// its own, so a low `pids` limit kills the probe on the spot — reported as the budget
    /// elapsing, a 24-second timeout that took no time.
    #[test]
    fn a_probe_the_host_stopped_is_not_reported_as_a_wait_that_never_happened() {
        let refused = deep(Err(ProbeFailure::NotStarted(
            "Resource temporarily unavailable".into(),
        )));
        assert_eq!(refused.status, CheckStatus::Fail);
        assert!(
            refused.detail.contains("Resource temporarily unavailable"),
            "the OS reason is what separates a pids limit from an OOM: {refused:?}"
        );
        assert!(
            !refused.detail.contains(&format!("{:?}", probe_budget())),
            "no wait happened, so the budget must not be quoted: {refused:?}"
        );

        // Both outcomes stopped short of Xvfb, so neither may be answered with Xvfb's advice —
        // and they are not each other's: one is the host refusing, one is glass unwinding.
        let vanished = deep(Err(ProbeFailure::Vanished));
        let (a, b) = (
            refused.remedy.as_deref().expect("a remedy"),
            vanished.remedy.as_deref().expect("a remedy"),
        );
        for r in [a, b] {
            assert!(
                !r.contains(XVFB_START_HINT),
                "the probe never reached Xvfb: {r}"
            );
        }
        assert_ne!(
            a, b,
            "a refused probe and a panicked one want different repairs"
        );
        assert!(b.contains("panic"), "{b}");
    }

    /// The two ways the bounded wait can end, which used to be one `Err(_)` arm. Driven through
    /// the real receiver rather than the mapper, because the collapse was in the wiring.
    #[test]
    fn a_probe_thread_that_died_is_told_apart_from_one_still_running() {
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        drop(tx);
        assert_eq!(
            await_probe(&rx, Duration::from_secs(24)),
            Err(ProbeFailure::Vanished),
            "a dropped sender is a thread that unwound, and arrives at once"
        );

        // Held open and silent: the shape of an Xvfb that spawned and never reported.
        let (_tx, rx) = mpsc::channel::<Result<String, String>>();
        let budget = Duration::from_millis(20);
        assert_eq!(
            await_probe(&rx, budget),
            Err(ProbeFailure::TimedOut(budget))
        );
    }

    /// The probe's own success and failure answers, which the wait must pass through untouched.
    #[test]
    fn the_wait_carries_the_probes_own_answer() {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(":99".into())).expect("send");
        assert_eq!(await_probe(&rx, Duration::from_secs(1)), Ok(":99".into()));

        let (tx, rx) = mpsc::channel();
        tx.send(Err("Xvfb exited during startup".into()))
            .expect("send");
        assert_eq!(
            await_probe(&rx, Duration::from_secs(1)),
            Err(ProbeFailure::Failed("Xvfb exited during startup".into()))
        );
    }

    /// glass#373: an MCP client that hands glass-mcp no PATH leaves nothing to look `Xvfb` up in.
    /// "not found" sends the user to install a package that is very likely already there.
    #[test]
    fn an_unset_path_is_not_reported_as_a_missing_xvfb() {
        let cs = self_spawn(&Resolved::NoSearchPath);
        let xvfb = cs.iter().find(|c| c.name == "Xvfb").expect("an Xvfb check");
        assert_eq!(xvfb.status, CheckStatus::Fail);
        assert!(xvfb.detail.contains("PATH"), "{:?}", xvfb.detail);
        let remedy = xvfb.remedy.as_deref().expect("a remedy");
        assert!(
            remedy.contains("GLASS_XVFB") && remedy.contains("PATH"),
            "{remedy}"
        );
        assert!(
            !remedy.contains("apt install"),
            "installing it again cannot restore a PATH: {remedy}"
        );
    }
}
