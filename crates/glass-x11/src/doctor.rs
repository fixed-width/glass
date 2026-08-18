//! Environment checks for the X11 backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`x11_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without a display.

use std::sync::mpsc;
use std::time::Duration;

use glass_core::{Check, CheckStatus, ProbeFailure};
use glass_exec_unix::{Resolved, resolve_bin};
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
        verdict: Result<(), NoAttach>,
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
            Ok(()) => Check::new(
                "GLASS_DISPLAY",
                CheckStatus::Ok,
                format!("{display} — reachable; glass will attach to it"),
            ),
            Err(no) => Check::new(
                "GLASS_DISPLAY",
                CheckStatus::Fail,
                format!("{display} — {}", no.cause),
            )
            .with_remedy(no.remedy),
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

/// What to check when Xvfb itself is what failed.
const XVFB_START_HINT: &str = "Xvfb is installed but produced no working display — check its \
     dependencies and permissions (the probe already waits out one retry of a stalled server)";

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
const FIX_THE_AUTHORITY: &str = "point XAUTHORITY at the cookie file for that display (`xauth \
     list` shows what it holds), or grant this user access with `xhost`";
const NAME_A_DISPLAY: &str = "GLASS_DISPLAY must be an X display name — `:42`, `:42.0` or `host:0`";
const PICK_A_SCREEN: &str = "name a screen that server has (`:42` or `:42.0`), or unset \
     GLASS_DISPLAY to self-spawn";

/// Whether glass can attach to `display`, and when it cannot, why — in the terms the remedy turns
/// on. The connection is closed again at once.
fn attach_verdict(display: &str) -> Result<(), NoAttach> {
    x11rb::connect(Some(display))
        .map(|_| ())
        .map_err(|e| no_attach(&e))
}

/// Classify a connect failure. `ConnectError` is `#[non_exhaustive]`, so anything not named here
/// falls to "nothing answered" — the common case, and the one that advice fits.
fn no_attach(e: &ConnectError) -> NoAttach {
    match e {
        // The server answered and turned glass away: an authority problem, not a display that is
        // down. XAUTHORITY unset or holding a stale cookie is the classic attach failure.
        ConnectError::SetupAuthenticate(_) | ConnectError::SetupFailed(_) => NoAttach {
            cause: format!("the server refused the connection ({e})"),
            remedy: FIX_THE_AUTHORITY,
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
    // Named and fallible, unlike a bare `spawn`, which panics when the OS refuses: `Xvfb::start`
    // spawns two more threads of its own, so a low `pids` limit stops this probe first.
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
            verdict: Ok(()),
        });
        assert_eq!(ok[0].status, CheckStatus::Ok);
        assert!(
            ok.iter().all(|c| c.name != "Xvfb"),
            "attach mode shouldn't require Xvfb"
        );

        let bad = x11_checks(&Mode::Attach {
            display: ":42",
            verdict: Err(no_attach(&ConnectError::UnknownError)),
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
            verdict: Err(no_attach(&refusal("Client is not authorized"))),
        });
        assert!(cs[0].detail.contains(":42"), "{:?}", cs[0].detail);
        assert!(
            cs[0].detail.contains("Client is not authorized"),
            "{:?}",
            cs[0].detail
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_live_display_is_reachable_and_an_unused_number_is_not() {
        let server = Xvfb::start("640x480x24").expect("Xvfb should start");
        assert_eq!(
            attach_verdict(&server.display),
            Ok(()),
            "the display we just started must be reachable"
        );
        drop(server);
        let no = attach_verdict(":9999").expect_err("an unused display number is not reachable");
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
        for failure in [
            ProbeFailure::NotStarted("Resource temporarily unavailable".into()),
            ProbeFailure::Vanished,
        ] {
            let check = deep(Err(failure.clone()));
            assert_eq!(check.status, CheckStatus::Fail);
            assert!(
                !check.detail.contains(&format!("{:?}", probe_budget())),
                "no wait happened: {check:?}"
            );
            assert!(
                check
                    .remedy
                    .as_deref()
                    .is_some_and(|r| r.contains("limits") && !r.contains("dependencies")),
                "the host stopped it, so Xvfb's dependencies are not the lead: {check:?}"
            );
        }
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
