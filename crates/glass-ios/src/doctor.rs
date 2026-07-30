//! `glass doctor` checks for the iOS Simulator backend: is a full Xcode install active,
//! does `xcrun simctl` work, is at least one iOS runtime downloaded, is an iPhone simulator
//! available to run apps on, and will the target glass resolves at start actually be driveable?
//!
//! Pure `build_checks(&Probe)` over observed state, plus the thin subprocess-probing
//! `checks(deep)` entry point the aggregator calls.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use glass_core::{Check, CheckStatus, GlassError};

use crate::device::{Resolve, SimDevice, parse_devices, resolve};
use crate::idb::companion::{IdbCompanion, companion_bin, companion_present_with};
use crate::simctl::Simctl;
use crate::target::wants;

/// Observed host state for the iOS doctor checks. Captured by [`checks`], consumed by the pure
/// [`build_checks`] so all branch logic is unit-testable without subprocesses.
pub struct Probe<'a> {
    /// `xcode-select -p` output: the active developer directory, if any.
    pub xcode_dir: Option<String>,
    /// Whether `xcrun simctl help` ran successfully.
    pub simctl_ok: bool,
    /// iOS runtime lines from `xcrun simctl list runtimes`.
    pub runtimes: &'a [String],
    /// iPhone simulator lines from `xcrun simctl list devices available`.
    pub iphones: &'a [String],
    /// What the device listing said about the target glass would drive.
    pub target: &'a TargetFacts,
}

/// What the doctor could learn about the simulator glass would drive at start.
///
/// Three-valued on purpose: a listing that failed is not a host with nothing booted, and reporting
/// one as the other is how a diagnostic sends an operator to fix something that is not broken.
#[derive(Debug, PartialEq, Eq)]
pub enum TargetFacts {
    /// The listing was read.
    Known {
        /// Name of an already-booted iOS-family simulator (an iPad counts — `resolve` drives any
        /// iOS-family device, and only *prefers* an iPhone), if any.
        booted: Option<String>,
        /// `GLASS_IOS_UDID`, when set: glass attaches to that udid **without booting it**, so a
        /// shut-down one makes every call fail against a dead target.
        pinned_udid: Option<String>,
        /// Whether `pinned_udid` names a device the listing reports as booted.
        pinned_is_booted: bool,
    },
    /// The listing could not be read, carrying why.
    Unknown(String),
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
        return Check::new("device", CheckStatus::Fail, "no iPhone simulator available").with_remedy(
            "create one in Xcode (Window > Devices and Simulators) or with `xcrun simctl create`",
        );
    }
    let n = p.iphones.len();
    match p.target {
        // Nothing booted is the ordinary cold state, not a finding: `SimTarget::from_env` boots one
        // with `bootstatus -b` at start. Warning here would prescribe a command glass runs itself.
        TargetFacts::Known {
            pinned_udid: None,
            booted,
            ..
        } => Check::new(
            "device",
            CheckStatus::Ok,
            match booted {
                Some(name) => format!("{n} iPhone simulator(s) available, booted: {name}"),
                None => format!(
                    "{n} iPhone simulator(s) available, none booted (glass boots one at start)"
                ),
            },
        ),
        // A pinned udid is the one case the start path will not boot for you: `resolve` returns it
        // unconditionally, so a shut-down one is a dead target and every call fails against it.
        TargetFacts::Known {
            pinned_udid: Some(udid),
            pinned_is_booted: false,
            ..
        } => Check::new(
            "device",
            CheckStatus::Fail,
            format!(
                "GLASS_IOS_UDID pins {udid}, which is not booted — glass attaches to a pinned udid \
                 without booting it, so every call would fail against a dead target"
            ),
        )
        .with_remedy(format!(
            "boot it with `xcrun simctl boot {udid}`, or unset GLASS_IOS_UDID and let glass pick"
        )),
        TargetFacts::Known {
            pinned_udid: Some(udid),
            ..
        } => Check::new(
            "device",
            CheckStatus::Ok,
            format!(
                "{n} iPhone simulator(s) available; GLASS_IOS_UDID pins {udid}, which is booted"
            ),
        ),
        TargetFacts::Unknown(cause) => Check::new(
            "device",
            CheckStatus::Warn,
            format!("{n} iPhone simulator(s) available; could not tell what is booted: {cause}"),
        )
        .with_remedy("run `xcrun simctl list devices available --json` and check it parses"),
    }
}

/// Deadline for each doctor probe. Doctor reports on the host rather than driving it, so every
/// probe here is a fast query; a tool that does not answer in this long is itself the finding.
const PROBE_BUDGET: Duration = Duration::from_secs(10);

/// Build the iOS doctor checks by probing the host with real `xcrun`/`xcode-select`
/// calls. Best-effort: a missing tool simply makes the corresponding check report
/// not-ok with a remedy, rather than failing this function. `_deep` is accepted for
/// signature parity with the other backends' doctors; iOS has no expensive deep probe.
pub fn checks(_deep: bool) -> Vec<Check> {
    // Bounded like every other one-shot: doctor's job is to report, and a doctor that hangs on a
    // wedged tool reports nothing at all. A timeout lands in the same `None` as a missing tool,
    // so the check still says not-ok with its remedy.
    let mut xcode_select = Command::new("xcode-select");
    xcode_select.arg("-p");
    let xcode_dir = glass_core::run_bounded(&mut xcode_select, PROBE_BUDGET, "xcode-select:-p")
        .inspect_err(|e| eprintln!("glass-ios doctor: {e}"))
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    // Labelled per probe, not "doctor probe": a timeout has to say whether `help`, `list runtimes`
    // or `list devices` hung. The error is logged rather than dropped, because `.ok()` folds a
    // timeout into the same `None` as a missing tool, and the resulting check then recommends
    // installing Xcode to someone whose Xcode is fine but whose CoreSimulator is wedged.
    let simctl_out = |args: &[&str]| {
        let mut cmd = Command::new("xcrun");
        cmd.args(args);
        glass_core::run_bounded(&mut cmd, PROBE_BUDGET, &format!("xcrun:{}", args.join(" ")))
            .inspect_err(|e| eprintln!("glass-ios doctor: {e}"))
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

    let target = gather_target(&simctl_out, &|k| std::env::var(k).ok());

    build_checks(&Probe {
        xcode_dir,
        simctl_ok,
        runtimes: &runtimes,
        iphones: &iphones,
        target: &target,
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
const SELF_TEST_ARG: &str = "--version";
/// Backstop only: `--version` returns near-instantly, so this bounds a wedged/hung binary.
const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for the self-test child to exit.
const SELF_TEST_POLL: Duration = Duration::from_millis(50);

/// Run the companion's bounded `--version` self-test: does the binary actually execute? Used
/// only when no simulator is booted (so a real spawn isn't possible without booting one).
/// Captures stderr to a temp file — a file can't fill and block, mirroring `IdbCompanion` —
/// and surfaces it as the cause on failure. Success ⇒ [`CompanionProbe::SelfTestOk`].
fn self_test() -> CompanionProbe {
    self_test_with(&companion_bin(&|k| std::env::var(k).ok()))
}

/// [`self_test`] against an explicit binary path — the testable seam (no env / no real
/// companion needed).
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
            return CompanionProbe::SelfTestFailed(format!("spawn {bin} {SELF_TEST_ARG}: {e}"));
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
fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Is the companion binary resolvable — `GLASS_IDB_COMPANION` naming an existing file, or
/// `idb_companion` found on `PATH` or in Homebrew's standard prefixes? The passive
/// (non-`--deep`) presence signal, and the `NotFound` gate for [`probe_companion`]. Uses the
/// same [`companion_bin`] resolution the runtime spawn does.
pub fn companion_present() -> bool {
    companion_present_with(&|k| std::env::var(k).ok(), &|p| p.is_file())
}

/// The `--deep` iOS companion health probe: does `idb_companion` actually start? Reuses the
/// real runtime spawn path against an *already-booted* simulator — never booting one, so it
/// stays bounded (the spawn carries its own 10s socket deadline) and non-mutating. When no
/// simulator is booted, falls back to the bounded [`self_test`] so `--deep` still yields a
/// signal. Only the aggregator's macOS-only ios section calls this.
pub fn probe_companion() -> CompanionProbe {
    if !companion_present() {
        return CompanionProbe::NotFound;
    }
    match booted_udid() {
        Some(udid) => match IdbCompanion::spawn(&udid) {
            // Dropping the companion kills+reaps the child and removes its socket.
            Ok(companion) => {
                drop(companion);
                CompanionProbe::Started
            }
            // The error already embeds the companion's captured stderr; strip the redundant
            // `GlassError::Backend` Display prefix (the mapping frames it "failed to start: …").
            Err(e) => CompanionProbe::FailedToStart(spawn_cause(e)),
        },
        None => self_test(),
    }
}

/// The human cause from a failed [`IdbCompanion::spawn`], stripped of `GlassError::Backend`'s
/// `"backend error: "` Display prefix — the doctor already frames it as "failed to start: …",
/// so the prefix would read redundantly in user-facing output. Any other variant falls back
/// to its full Display.
fn spawn_cause(e: GlassError) -> String {
    match e {
        GlassError::Backend(msg) => msg,
        other => other.to_string(),
    }
}

/// UDID of an already-booted iOS simulator, or `None` if none is booted (or the device list
/// can't be read). A `simctl`/parse failure yields `None` so [`probe_companion`] falls back
/// to the self-test rather than erroring.
fn booted_udid() -> Option<String> {
    let list = Simctl::new()
        .run(&["list", "devices", "available", "--json"])
        .ok()?;
    booted_from(&parse_devices(&list).ok()?)
}

/// Read the device listing once and answer what the start path would do with it.
///
/// Every failure to read it becomes [`TargetFacts::Unknown`] carrying the cause, rather than the
/// `None` that means "nothing is booted": a timed-out or unparseable listing reported as "none
/// booted" is a remedy the operator cannot follow to green.
fn gather_target(
    simctl_out: &dyn Fn(&[&str]) -> Option<String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> TargetFacts {
    let Some(json) = simctl_out(&["simctl", "list", "devices", "available", "--json"]) else {
        return TargetFacts::Unknown("`xcrun simctl list devices --json` did not answer".into());
    };
    match parse_devices(&json) {
        Ok(devices) => target_from(&devices, wants(env).0),
        Err(e) => TargetFacts::Unknown(e.to_string()),
    }
}

/// The pure half of [`gather_target`]: what the listing says about the target glass would drive.
fn target_from(devices: &[SimDevice], pinned_udid: Option<String>) -> TargetFacts {
    let booted = booted_name(devices);
    let pinned_is_booted = pinned_udid
        .as_deref()
        .is_some_and(|u| devices.iter().any(|d| d.udid == u && d.state == "Booted"));
    TargetFacts::Known {
        booted,
        pinned_udid,
        pinned_is_booted,
    }
}

/// Name of the already-booted simulator [`booted_from`] would attach to — the same selection a
/// driving call makes when `GLASS_IOS_UDID` is unset (a pinned udid bypasses this entirely).
fn booted_name(devices: &[SimDevice]) -> Option<String> {
    let udid = booted_from(devices)?;
    devices
        .iter()
        .find(|d| d.udid == udid)
        .map(|d| d.name.clone())
}

/// Pure booted-sim selection: `resolve(_, None, None)` returns `Attach` iff an iOS sim is
/// already booted, so `Attach` is exactly "spawn against it without booting"; `Boot`/`Error`
/// mean nothing is booted. The testable seam for [`booted_udid`].
fn booted_from(devices: &[SimDevice]) -> Option<String> {
    match resolve(devices, None, None) {
        Resolve::Attach(udid) => Some(udid),
        Resolve::Boot(_) | Resolve::Error(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::SimDevice;

    #[test]
    fn spawn_cause_strips_backend_error_prefix() {
        // `FailedToStart` detail is framed "failed to start: {cause}", so the
        // `GlassError::Backend` Display prefix ("backend error: ") would read redundantly.
        assert_eq!(
            spawn_cause(GlassError::Backend(
                "idb_companion exited (exit status: 1) before serving its socket".into()
            )),
            "idb_companion exited (exit status: 1) before serving its socket"
        );
    }

    fn dev(udid: &str, name: &str, state: &str) -> SimDevice {
        dev_on(
            udid,
            name,
            state,
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5",
        )
    }

    fn dev_on(udid: &str, name: &str, state: &str, runtime: &str) -> SimDevice {
        SimDevice {
            udid: udid.into(),
            name: name.into(),
            state: state.into(),
            runtime: runtime.into(),
            is_available: true,
        }
    }

    #[test]
    fn booted_from_picks_a_booted_ios_sim() {
        let devices = vec![
            dev("AAA", "iPhone 17", "Shutdown"),
            dev("BBB", "iPhone 17 Pro", "Booted"),
        ];
        assert_eq!(booted_from(&devices), Some("BBB".to_string()));
    }

    #[test]
    fn booted_from_is_none_when_nothing_is_booted() {
        let devices = vec![
            dev("AAA", "iPhone 17", "Shutdown"),
            dev("CCC", "iPhone 15", "Shutdown"),
        ];
        assert_eq!(booted_from(&devices), None);
    }

    /// A read listing, with an optional booted device and an optional `GLASS_IOS_UDID` pin.
    fn known(booted: Option<&str>, pinned: Option<(&str, bool)>) -> TargetFacts {
        TargetFacts::Known {
            booted: booted.map(Into::into),
            pinned_udid: pinned.map(|(u, _)| u.to_string()),
            pinned_is_booted: pinned.is_some_and(|(_, b)| b),
        }
    }

    fn device_line(target: &TargetFacts) -> Check {
        let runtimes = vec!["iOS 26.5".to_string()];
        let iphones = vec!["iPhone 17".to_string(), "iPhone 17 Pro".to_string()];
        let p = Probe {
            xcode_dir: Some("/Applications/Xcode.app/Contents/Developer".into()),
            simctl_ok: true,
            runtimes: &runtimes,
            iphones: &iphones,
            target,
        };
        build_checks(&p)
            .into_iter()
            .find(|c| c.name == "device")
            .unwrap()
    }

    #[test]
    fn nothing_booted_is_not_a_finding_because_glass_boots_one_at_start() {
        // `SimTarget::from_env` runs `bootstatus -b` when nothing is booted, so warning here would
        // prescribe a command glass runs itself, on a host with nothing wrong with it.
        let c = device_line(&known(None, None));
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(c.detail.contains("boots one at start"), "{}", c.detail);
    }

    #[test]
    fn a_booted_simulator_is_named_so_the_operator_knows_which_one() {
        let c = device_line(&known(Some("iPhone 17 Pro"), None));
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(c.detail.contains("iPhone 17 Pro"), "{}", c.detail);
    }

    #[test]
    fn a_pinned_udid_that_is_not_booted_is_the_one_state_start_will_not_fix() {
        // `resolve` returns a pinned udid without checking its state and without booting it, so
        // glass attaches to a dead target and every later call fails against it.
        let c = device_line(&known(Some("iPhone 17 Pro"), Some(("DEAD-UDID", false))));
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(c.detail.contains("DEAD-UDID"), "{}", c.detail);
        assert!(
            c.remedy.as_deref().unwrap().contains("GLASS_IOS_UDID"),
            "{:?}",
            c.remedy
        );
    }

    #[test]
    fn a_pinned_udid_that_is_booted_is_reported_not_warned_about() {
        let c = device_line(&known(None, Some(("LIVE-UDID", true))));
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(c.detail.contains("LIVE-UDID"), "{}", c.detail);
    }

    #[test]
    fn a_listing_that_could_not_be_read_says_so_instead_of_none_booted() {
        // The failure this shape exists to prevent: a timed-out or unparseable listing reported as
        // "nothing booted" is a remedy the operator cannot follow to green.
        let c = device_line(&TargetFacts::Unknown(
            "simctl list JSON parse failed: eof".into(),
        ));
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.contains("parse failed"), "{}", c.detail);
        assert!(
            !c.remedy.as_deref().unwrap().contains("simctl boot"),
            "must not prescribe booting: {:?}",
            c.remedy
        );
    }

    #[test]
    fn booted_name_reports_the_device_a_driving_call_would_attach_to() {
        // Two booted devices and a non-iOS one: a naive "first booted" pick names the watch or the
        // wrong simulator, which is not what `resolve` — and so a driving call — would attach to.
        let devices = vec![
            dev_on(
                "WATCH",
                "Apple Watch Series 10",
                "Booted",
                "com.apple.CoreSimulator.SimRuntime.watchOS-10-4",
            ),
            dev("IPAD", "iPad Pro 13-inch", "Booted"),
            dev("PHONE", "iPhone 17 Pro", "Booted"),
        ];
        assert_eq!(booted_name(&devices), Some("iPhone 17 Pro".to_string()));
        // A booted watch is not a target: nothing iOS-family is booted here.
        assert_eq!(booted_name(&devices[..1]), None);
    }

    #[test]
    fn a_pin_is_only_booted_when_the_listing_says_that_exact_udid_is() {
        let devices = vec![
            dev("AAA", "iPhone 17", "Shutdown"),
            dev("BBB", "iPhone 17 Pro", "Booted"),
        ];
        assert_eq!(
            target_from(&devices, Some("AAA".into())),
            known(Some("iPhone 17 Pro"), Some(("AAA", false)))
        );
        assert_eq!(
            target_from(&devices, Some("BBB".into())),
            known(Some("iPhone 17 Pro"), Some(("BBB", true)))
        );
    }

    #[test]
    fn a_listing_that_does_not_answer_is_unknown_not_empty() {
        let facts = gather_target(&|_| None, &|_| None);
        assert!(matches!(facts, TargetFacts::Unknown(_)), "{facts:?}");
    }

    #[test]
    fn a_read_listing_carries_the_pin_from_the_environment() {
        let json = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-5":[
            {"udid":"BBB","name":"iPhone 17 Pro","state":"Booted","isAvailable":true}]}}"#;
        let facts = gather_target(&|_| Some(json.to_string()), &|k| {
            (k == "GLASS_IOS_UDID").then(|| "BBB".to_string())
        });
        assert_eq!(facts, known(Some("iPhone 17 Pro"), Some(("BBB", true))));
    }

    #[test]
    fn all_green_when_fully_configured() {
        let runtimes = vec!["iOS 26.5".to_string()];
        let iphones = vec!["iPhone 17".to_string()];
        let p = Probe {
            xcode_dir: Some("/Applications/Xcode.app/Contents/Developer".into()),
            simctl_ok: true,
            runtimes: &runtimes,
            iphones: &iphones,
            target: &known(Some("iPhone 17"), None),
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
            target: &known(None, None),
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
            target: &known(None, None),
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
            target: &known(None, None),
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
        let bin = script.to_str().unwrap();

        // Retry past a transient ETXTBSY ("Text file busy", os error 26): `cargo test` runs
        // tests on parallel threads, and a sibling thread's `Command::spawn` (fork) can
        // momentarily inherit the write fd of the just-written fixture, so exec'ing it races
        // until that fork execs and closes the fd. This affects only a freshly-written test
        // fixture, never the already-installed real idb_companion, so the retry lives here
        // rather than in `self_test_with`.
        let mut cause = None;
        for _ in 0..100 {
            match self_test_with(bin) {
                CompanionProbe::SelfTestFailed(c) if c.contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                CompanionProbe::SelfTestFailed(c) => {
                    cause = Some(c);
                    break;
                }
                other => panic!("expected SelfTestFailed, got {other:?}"),
            }
        }
        let cause = cause.expect("self_test_with kept returning ETXTBSY after 100 retries");
        assert!(
            cause.contains("boom-from-stderr"),
            "cause missing stderr: {cause}"
        );
        assert!(cause.contains('3'), "cause missing exit status: {cause}");
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
