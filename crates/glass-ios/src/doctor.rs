//! `glass doctor` checks for the iOS Simulator backend: is a full Xcode install active,
//! does `xcrun simctl` work, is at least one iOS runtime downloaded, and is an iPhone
//! simulator available to run apps on?
//!
//! Pure `build_checks(&Probe)` over observed state, plus the thin subprocess-probing
//! `checks(deep)` entry point the aggregator calls.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use glass_core::{Check, CheckStatus};

use crate::idb::companion::companion_bin;

/// Observed host state for the iOS doctor checks. Captured by `run`, consumed by the
/// pure `checks` so all branch logic is unit-testable without subprocesses.
pub struct Probe<'a> {
    /// `xcode-select -p` output: the active developer directory, if any.
    pub xcode_dir: Option<String>,
    /// Whether `xcrun simctl help` ran successfully.
    pub simctl_ok: bool,
    /// iOS runtime lines from `xcrun simctl list runtimes`.
    pub runtimes: &'a [String],
    /// iPhone simulator lines from `xcrun simctl list devices available`.
    pub iphones: &'a [String],
}

const INSTALL_XCODE_REMEDY: &str =
    "install full Xcode and run `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`";

/// Build the iOS doctor checks from observed state. Pure — no OS calls — so all branch
/// logic is unit-testable without subprocesses.
fn build_checks(p: &Probe) -> Vec<Check> {
    vec![
        xcode_check(p),
        simctl_check(p),
        runtime_check(p),
        device_check(p),
    ]
}

fn xcode_check(p: &Probe) -> Check {
    match &p.xcode_dir {
        Some(dir) if dir.contains("Xcode.app") => Check::new(
            "xcode",
            CheckStatus::Ok,
            format!("active developer dir: {dir}"),
        ),
        Some(dir) => Check::new(
            "xcode",
            CheckStatus::Fail,
            format!("active developer dir is Command Line Tools only: {dir}"),
        )
        .with_remedy(INSTALL_XCODE_REMEDY),
        None => Check::new("xcode", CheckStatus::Fail, "no active developer directory")
            .with_remedy("install Xcode from the App Store"),
    }
}

fn simctl_check(p: &Probe) -> Check {
    if p.simctl_ok {
        Check::new("simctl", CheckStatus::Ok, "xcrun simctl is available")
    } else {
        Check::new("simctl", CheckStatus::Fail, "xcrun simctl is unavailable")
            .with_remedy(INSTALL_XCODE_REMEDY)
    }
}

fn runtime_check(p: &Probe) -> Check {
    if p.runtimes.is_empty() {
        Check::new(
            "runtime",
            CheckStatus::Fail,
            "no iOS simulator runtime installed",
        )
        .with_remedy("download one with `xcodebuild -downloadPlatform iOS`")
    } else {
        Check::new(
            "runtime",
            CheckStatus::Ok,
            format!("iOS runtimes: {}", p.runtimes.join(", ")),
        )
    }
}

fn device_check(p: &Probe) -> Check {
    if p.iphones.is_empty() {
        Check::new("device", CheckStatus::Fail, "no iPhone simulator available").with_remedy(
            "create one in Xcode (Window > Devices and Simulators) or with `xcrun simctl create`",
        )
    } else {
        Check::new(
            "device",
            CheckStatus::Ok,
            format!("{} iPhone simulator(s) available", p.iphones.len()),
        )
    }
}

/// Build the iOS doctor checks by probing the host with real `xcrun`/`xcode-select`
/// calls. Best-effort: a missing tool simply makes the corresponding check report
/// not-ok with a remedy, rather than failing this function. `_deep` is accepted for
/// signature parity with the other backends' doctors; iOS has no expensive deep probe.
pub fn checks(_deep: bool) -> Vec<Check> {
    let xcode_dir = Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let simctl_out = |args: &[&str]| {
        Command::new("xcrun")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };

    let simctl_ok = simctl_out(&["simctl", "help"]).is_some();
    let runtimes: Vec<String> = simctl_out(&["simctl", "list", "runtimes"])
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("iOS"))
        .map(|l| l.trim().to_string())
        .collect();
    let iphones: Vec<String> = simctl_out(&["simctl", "list", "devices", "available"])
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("iPhone"))
        .map(|l| l.trim().to_string())
        .collect();

    build_checks(&Probe {
        xcode_dir,
        simctl_ok,
        runtimes: &runtimes,
        iphones: &iphones,
    })
}

/// Outcome of the `--deep` iOS `idb_companion` health probe (see [`probe_companion`]). Pure
/// data — the aggregator (`glass-mcp`) maps it to a `Check`. `NotFound` means the binary is
/// unresolvable; `Started`/`FailedToStart` come from a real spawn against an already-booted
/// simulator; `SelfTestOk`/`SelfTestFailed` come from the bounded `--version` fallback used
/// when no simulator is booted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanionProbe {
    NotFound,
    Started,
    FailedToStart(String),
    SelfTestOk,
    SelfTestFailed(String),
}

/// Self-test flag. `idb_companion --version` needs no simulator, exits promptly with status
/// 0, and prints a build-info line (confirmed against idb-companion 1.1.8). It may print a
/// benign objc dyld warning to *stderr* while still exiting 0, so success keys on the exit
/// status — never on empty stderr.
#[cfg_attr(not(test), allow(dead_code))]
const SELF_TEST_ARG: &str = "--version";
/// Backstop only: `--version` returns near-instantly, so this bounds a wedged/hung binary.
#[cfg_attr(not(test), allow(dead_code))]
const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for the self-test child to exit.
#[cfg_attr(not(test), allow(dead_code))]
const SELF_TEST_POLL: Duration = Duration::from_millis(50);

/// Run the companion's bounded `--version` self-test: does the binary actually execute? Used
/// only when no simulator is booted (so a real spawn isn't possible without booting one).
/// Captures stderr to a temp file — a file can't fill and block, mirroring `IdbCompanion` —
/// and surfaces it as the cause on failure. Success ⇒ [`CompanionProbe::SelfTestOk`].
///
/// Not yet called from `checks()` — the probe orchestration that picks between a real spawn
/// and this fallback lands in a follow-up change.
#[allow(dead_code)]
fn self_test() -> CompanionProbe {
    self_test_with(&companion_bin(&|k| std::env::var(k).ok()))
}

/// [`self_test`] against an explicit binary path — the testable seam (no env / no real
/// companion needed).
#[cfg_attr(not(test), allow(dead_code))]
fn self_test_with(bin: &str) -> CompanionProbe {
    // A uniquely-named temp file (rather than a name keyed on this process's pid) — several
    // self-tests can run concurrently in one process, e.g. this module's own tests running in
    // parallel, and a pid-only name would let them collide on the same log file.
    let log = match tempfile::NamedTempFile::new() {
        Ok(f) => f,
        Err(e) => return CompanionProbe::SelfTestFailed(format!("create self-test log: {e}")),
    };
    let stderr = match log.reopen() {
        Ok(f) => f,
        Err(e) => return CompanionProbe::SelfTestFailed(format!("create self-test log: {e}")),
    };
    let mut child = match Command::new(bin)
        .arg(SELF_TEST_ARG)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return CompanionProbe::SelfTestFailed(format!("spawn {bin} {SELF_TEST_ARG}: {e}"))
        }
    };
    let deadline = Instant::now() + SELF_TEST_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break CompanionProbe::SelfTestOk,
            Ok(Some(status)) => {
                let stderr = read_trimmed(log.path()).filter(|s| !s.is_empty());
                break CompanionProbe::SelfTestFailed(match stderr {
                    Some(s) => format!("{SELF_TEST_ARG} exited {status}: {s}"),
                    None => format!("{SELF_TEST_ARG} exited {status}"),
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break CompanionProbe::SelfTestFailed(format!(
                    "{SELF_TEST_ARG} timed out after {SELF_TEST_TIMEOUT:?}"
                ));
            }
            Ok(None) => std::thread::sleep(SELF_TEST_POLL),
            Err(e) => break CompanionProbe::SelfTestFailed(format!("try_wait: {e}")),
        }
    }
    // `log` (a `NamedTempFile`) removes its file on drop here.
}

/// Read a file and trim it, or `None` if it can't be read.
#[cfg_attr(not(test), allow(dead_code))]
fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_green_when_fully_configured() {
        let runtimes = vec!["iOS 26.5".to_string()];
        let iphones = vec!["iPhone 17".to_string()];
        let p = Probe {
            xcode_dir: Some("/Applications/Xcode.app/Contents/Developer".into()),
            simctl_ok: true,
            runtimes: &runtimes,
            iphones: &iphones,
        };
        let cs = build_checks(&p);
        assert!(cs.iter().all(|c| c.status == CheckStatus::Ok), "{cs:?}");
    }

    #[test]
    fn flags_command_line_tools_only() {
        let p = Probe {
            xcode_dir: Some("/Library/Developer/CommandLineTools".into()),
            simctl_ok: false,
            runtimes: &[],
            iphones: &[],
        };
        let cs = build_checks(&p);
        let xcode = cs.iter().find(|c| c.name == "xcode").unwrap();
        assert_eq!(xcode.status, CheckStatus::Fail);
        assert!(
            xcode.remedy.as_deref().unwrap().contains("full Xcode"),
            "{:?}",
            xcode.remedy
        );
        // CLT-only also means `simctl` itself is unavailable — assert that check fails too,
        // not just `xcode`.
        assert_eq!(
            cs.iter().find(|c| c.name == "simctl").unwrap().status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn no_active_developer_directory_fails_with_install_xcode_remedy() {
        let p = Probe {
            xcode_dir: None,
            simctl_ok: false,
            runtimes: &[],
            iphones: &[],
        };
        let cs = build_checks(&p);
        let xcode = cs.iter().find(|c| c.name == "xcode").unwrap();
        assert_eq!(xcode.status, CheckStatus::Fail);
        assert!(
            xcode.remedy.as_deref().unwrap().contains("Xcode"),
            "{:?}",
            xcode.remedy
        );
    }

    #[test]
    fn flags_missing_runtime_and_device() {
        let p = Probe {
            xcode_dir: Some("/Applications/Xcode.app/Contents/Developer".into()),
            simctl_ok: true,
            runtimes: &[],
            iphones: &[],
        };
        let cs = build_checks(&p);
        assert_eq!(
            cs.iter().find(|c| c.name == "runtime").unwrap().status,
            CheckStatus::Fail
        );
        assert_eq!(
            cs.iter().find(|c| c.name == "device").unwrap().status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn self_test_ok_when_binary_exits_zero() {
        // `/bin/echo --version` prints and exits 0 regardless of args — stands in for a
        // healthy idb_companion whose real `--version` also exits 0.
        assert_eq!(self_test_with("/bin/echo"), CompanionProbe::SelfTestOk);
    }

    #[test]
    fn self_test_fails_and_captures_cause_on_nonzero_exit() {
        // A fake binary that writes to stderr and exits non-zero: the probe must surface both
        // the exit status and the captured stderr as the cause. idb_companion prints a benign
        // objc warning to stderr while still exiting 0, so success keys on exit status — this
        // asserts the *failure* branch does read stderr for the cause.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake_companion");
        std::fs::write(&script, "#!/bin/sh\necho 'boom-from-stderr' >&2\nexit 3\n").expect("write");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        match self_test_with(script.to_str().unwrap()) {
            CompanionProbe::SelfTestFailed(cause) => {
                assert!(
                    cause.contains("boom-from-stderr"),
                    "cause missing stderr: {cause}"
                );
                assert!(cause.contains('3'), "cause missing exit status: {cause}");
            }
            other => panic!("expected SelfTestFailed, got {other:?}"),
        }
    }

    #[test]
    fn self_test_fails_when_binary_is_unspawnable() {
        match self_test_with("/nonexistent/definitely-not-a-binary") {
            CompanionProbe::SelfTestFailed(cause) => {
                assert!(
                    cause.contains("spawn"),
                    "cause should name the spawn failure: {cause}"
                );
            }
            other => panic!("expected SelfTestFailed, got {other:?}"),
        }
    }
}
