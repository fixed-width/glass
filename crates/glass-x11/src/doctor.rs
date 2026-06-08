//! Environment checks for the X11 backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`x11_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without a display.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use glass_core::{Check, CheckStatus};

use crate::xvfb::Xvfb;

/// Probe the X11 backend's environment. `deep` additionally spawns and tears down a
/// private Xvfb (when in self-spawn mode) to prove it actually starts.
pub fn checks(deep: bool) -> Vec<Check> {
    let glass_display = std::env::var("GLASS_DISPLAY").ok();
    let gd = glass_display.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let xvfb = resolve_bin(&glass_core::tool_path("GLASS_XVFB", "Xvfb"));
    let attach_reachable = gd.map(|d| can_connect(&normalize_display(d)));
    let deep_spawn = (deep && gd.is_none()).then(probe_xvfb);
    x11_checks(gd, xvfb.as_deref(), attach_reachable, deep_spawn)
}

/// Pure: build the X11 checks from gathered facts.
fn x11_checks(
    glass_display: Option<&str>,
    xvfb: Option<&Path>,
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
                Some(p) => Check::new("Xvfb", CheckStatus::Ok, p.display().to_string()),
                None => Check::new("Xvfb", CheckStatus::Fail, "not found").with_remedy(
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
                _ => Check::new("GLASS_DISPLAY", CheckStatus::Fail, format!("{d} — cannot connect"))
                    .with_remedy(
                        "start that display (e.g. `./scripts/sandbox-xvfb.sh start` for :42) \
                         or unset GLASS_DISPLAY to self-spawn",
                    ),
            });
        }
    }
    checks
}

/// First executable named `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Resolve a configured binary to an existing path: an explicit path (contains `/`) must
/// be an existing file; a bare name is looked up on `PATH`.
fn resolve_bin(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        p.is_file().then_some(p)
    } else {
        which(bin)
    }
}

fn normalize_display(d: &str) -> String {
    if d.starts_with(':') {
        d.to_string()
    } else {
        format!(":{d}")
    }
}

fn can_connect(display: &str) -> bool {
    x11rb::connect(Some(display)).is_ok()
}

/// Spawn a private Xvfb and tear it down, with a timeout so a wedged Xvfb can't hang
/// doctor. Returns the display it came up on, or an error string.
fn probe_xvfb() -> Result<String, String> {
    let screen = std::env::var("GLASS_XVFB_SCREEN").unwrap_or_else(|_| "1280x800x24".into());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The Xvfb is dropped at the end of `map` (after we read its display),
        // tearing the test display back down.
        let _ = tx.send(Xvfb::start(&screen).map(|x| x.display.clone()).map_err(|e| e.to_string()));
    });
    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(r) => r,
        Err(_) => Err("Xvfb did not become ready within 8s".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_spawn_with_xvfb_present_is_ok() {
        let p = PathBuf::from("/usr/bin/Xvfb");
        let cs = x11_checks(None, Some(&p), None, None);
        assert_eq!(cs[0].name, "GLASS_DISPLAY");
        assert_eq!(cs[0].status, CheckStatus::Ok);
        assert_eq!(cs[1].name, "Xvfb");
        assert_eq!(cs[1].status, CheckStatus::Ok);
        assert_eq!(cs[1].detail, "/usr/bin/Xvfb");
    }

    #[test]
    fn self_spawn_without_xvfb_fails_with_remedy() {
        let cs = x11_checks(None, None, None, None);
        let xvfb = cs.iter().find(|c| c.name == "Xvfb").unwrap();
        assert_eq!(xvfb.status, CheckStatus::Fail);
        assert!(xvfb.remedy.as_deref().unwrap().contains("apt install xvfb"));
    }

    #[test]
    fn attach_reachable_is_ok_unreachable_fails() {
        let ok = x11_checks(Some(":42"), None, Some(true), None);
        assert_eq!(ok[0].status, CheckStatus::Ok);
        assert!(ok.iter().all(|c| c.name != "Xvfb"), "attach mode shouldn't require Xvfb");

        let bad = x11_checks(Some(":42"), None, Some(false), None);
        assert_eq!(bad[0].status, CheckStatus::Fail);
        assert!(bad[0].remedy.is_some());
    }

    #[test]
    fn deep_spawn_failure_is_reported() {
        let cs = x11_checks(None, Some(Path::new("/usr/bin/Xvfb")), None, Some(Err("boom".into())));
        let deep = cs.iter().find(|c| c.name == "Xvfb spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail);
        assert_eq!(deep.detail, "boom");
    }
}
