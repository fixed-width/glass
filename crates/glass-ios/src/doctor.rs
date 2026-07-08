//! `glass doctor` checks for the iOS Simulator backend: is a full Xcode install active,
//! does `xcrun simctl` work, is at least one iOS runtime downloaded, and is an iPhone
//! simulator available to run apps on?
//!
//! Pure `build_checks(&Probe)` over observed state, plus the thin subprocess-probing
//! `checks(deep)` entry point the aggregator calls.

use glass_core::{Check, CheckStatus};

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
    use std::process::Command;

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
}
