//! Environment checks for the X11 backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`x11_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without a display.

use std::sync::mpsc;
use std::time::Duration;

use glass_core::{Check, CheckStatus};
use glass_exec_unix::{Resolved, resolve_bin};

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
    let gd = glass_display.map(str::trim).filter(|s| !s.is_empty());
    let xvfb = resolve_bin(
        &glass_core::tool_path("GLASS_XVFB", "Xvfb"),
        std::env::var_os("PATH").as_deref(),
    );
    let attach_reachable = gd.map(|d| can_connect(&crate::platform::normalize_display(d)));
    let deep_spawn = (deep && gd.is_none()).then(probe_xvfb);
    x11_checks(gd, &xvfb, attach_reachable, deep_spawn)
}

/// Pure: build the X11 checks from gathered facts.
fn x11_checks(
    glass_display: Option<&str>,
    xvfb: &Resolved,
    attach_reachable: Option<bool>,
    deep_spawn: Option<Result<String, String>>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    match glass_display {
        // Self-spawn mode: Xvfb is required.
        None => {
            checks.push(Check::new(
                "GLASS_DISPLAY",
                CheckStatus::Ok,
                "unset — glass will spawn a private headless Xvfb",
            ));
            checks.push(match xvfb {
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
            });
            if let Some(res) = deep_spawn {
                checks.push(match res {
                    Ok(d) => Check::new(
                        "Xvfb spawn (deep)",
                        CheckStatus::Ok,
                        format!("started and stopped {d}"),
                    ),
                    Err(e) => Check::new("Xvfb spawn (deep)", CheckStatus::Fail, e).with_remedy(
                        "Xvfb is installed but failed to start — check its dependencies/permissions",
                    ),
                });
            }
        }
        // Attach mode: the named display must be reachable.
        Some(d) => {
            checks.push(match attach_reachable {
                Some(true) => Check::new(
                    "GLASS_DISPLAY",
                    CheckStatus::Ok,
                    format!("{d} — reachable; glass will attach to it"),
                ),
                _ => Check::new(
                    "GLASS_DISPLAY",
                    CheckStatus::Fail,
                    format!("{d} — cannot connect"),
                )
                .with_remedy(
                    "start that display (e.g. `./scripts/sandbox-xvfb.sh start` for :42) \
                         or unset GLASS_DISPLAY to self-spawn",
                ),
            });
        }
    }
    checks
}

fn can_connect(display: &str) -> bool {
    x11rb::connect(Some(display)).is_ok()
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
/// doctor. Returns the display it came up on, or an error string.
fn probe_xvfb() -> Result<String, String> {
    let screen = std::env::var("GLASS_XVFB_SCREEN").unwrap_or_else(|_| "1280x800x24".into());
    let budget = probe_budget();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The Xvfb is dropped at the end of `map` (after we read its display),
        // tearing the test display back down.
        let _ = tx.send(
            Xvfb::start(&screen)
                .map(|x| x.display.clone())
                .map_err(|e| e.to_string()),
        );
    });
    match rx.recv_timeout(budget) {
        Ok(r) => r,
        Err(_) => Err(format!(
            "Xvfb did not become ready within {}s (including one retry of a stalled server)",
            budget.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn self_spawn_with_xvfb_present_is_ok() {
        let cs = x11_checks(
            None,
            &Resolved::Found(PathBuf::from("/usr/bin/Xvfb")),
            None,
            None,
        );
        assert_eq!(cs[0].name, "GLASS_DISPLAY");
        assert_eq!(cs[0].status, CheckStatus::Ok);
        assert_eq!(cs[1].name, "Xvfb");
        assert_eq!(cs[1].status, CheckStatus::Ok);
        assert_eq!(cs[1].detail, "/usr/bin/Xvfb");
    }

    #[test]
    fn self_spawn_without_xvfb_fails_with_remedy() {
        let cs = x11_checks(None, &Resolved::Absent, None, None);
        let xvfb = cs.iter().find(|c| c.name == "Xvfb").unwrap();
        assert_eq!(xvfb.status, CheckStatus::Fail);
        assert!(xvfb.remedy.as_deref().unwrap().contains("apt install xvfb"));
    }

    #[test]
    fn attach_reachable_is_ok_unreachable_fails() {
        let ok = x11_checks(Some(":42"), &Resolved::Absent, Some(true), None);
        assert_eq!(ok[0].status, CheckStatus::Ok);
        assert!(
            ok.iter().all(|c| c.name != "Xvfb"),
            "attach mode shouldn't require Xvfb"
        );

        let bad = x11_checks(Some(":42"), &Resolved::Absent, Some(false), None);
        assert_eq!(bad[0].status, CheckStatus::Fail);
        assert!(bad[0].remedy.is_some());
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_live_display_is_reachable_and_an_unused_number_is_not() {
        let server = Xvfb::start("640x480x24").expect("Xvfb should start");
        assert!(
            can_connect(&server.display),
            "the display we just started must be reachable"
        );
        drop(server);
        assert!(
            !can_connect(":9999"),
            "an unused display number must not read as reachable"
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
        let cs = x11_checks(None, &Resolved::NotExecutable(p), None, None);
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

    #[test]
    fn deep_spawn_failure_is_reported() {
        let cs = x11_checks(
            None,
            &Resolved::Found(PathBuf::from("/usr/bin/Xvfb")),
            None,
            Some(Err("boom".into())),
        );
        let deep = cs.iter().find(|c| c.name == "Xvfb spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail);
        assert_eq!(deep.detail, "boom");
    }
}
