//! `glass doctor` checks for the iOS Simulator backend: is a full Xcode install active,
//! does `xcrun simctl` work, is at least one iOS runtime downloaded, and is the target glass would
//! resolve at start actually driveable?
//!
//! Pure `build_checks(&Probe)` over observed state, plus the thin subprocess-probing
//! `checks(deep)` entry point the aggregator calls.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use glass_core::{Check, CheckStatus, GlassError};
use glass_exec_unix::Resolved;

use crate::device::{Resolve, SimDevice, parse_devices, resolve};
use crate::idb::companion::{CompanionFacts, IdbCompanion};
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
    /// What the device listing said about the target glass would drive.
    pub target: &'a TargetFacts,
}

/// What the doctor could learn about the simulator glass would drive at start.
///
/// Every arm is the outcome of the *real* resolution the start path runs ([`crate::device::resolve`],
/// with the same `GLASS_IOS_UDID` / `GLASS_IOS_DEVICE` preferences). A listing that could not be read
/// is its own arm: reported as "nothing booted", it is a remedy the operator cannot follow to green.
#[derive(Debug, PartialEq, Eq)]
pub enum TargetFacts {
    /// A booted device glass would attach to, and how many iOS-family simulators are available.
    Attaching { name: String, available: usize },
    /// Nothing booted; glass boots this one at start.
    WillBoot { name: String, available: usize },
    /// `GLASS_IOS_UDID` names a device the listing does not contain.
    PinnedMissing(String),
    /// `GLASS_IOS_UDID` names a device that is present but not booted. glass attaches to a pinned
    /// udid *without* booting it, so this is the one state the start path will not fix for you.
    PinnedNotBooted(String),
    /// `GLASS_IOS_UDID` names a booted device that is not an iOS simulator. glass attaches to it —
    /// `resolve` does not filter a pinned udid by runtime — and every iOS call then fails.
    PinnedNotIos { udid: String, name: String },
    /// Resolution failed outright. `named` records whether a `GLASS_IOS_DEVICE` preference was in
    /// play, because that changes the remedy from "create a simulator" to "fix the variable".
    Unresolvable { why: String, named: bool },
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
    let ok = |detail: String| Check::new("device", CheckStatus::Ok, detail);
    match p.target {
        // Nothing booted is the ordinary cold state, not a finding: `SimTarget::from_env` boots one
        // with `bootstatus -b` at start. Warning here would prescribe a command glass runs itself.
        TargetFacts::WillBoot { name, available } => ok(format!(
            "{available} iOS simulator(s) available, none booted (glass boots {name} at start)"
        )),
        TargetFacts::Attaching { name, available } => ok(format!(
            "{available} iOS simulator(s) available, booted: {name}"
        )),
        TargetFacts::PinnedNotBooted(udid) => Check::new(
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
        // Distinct from not-booted: `simctl boot` on a udid the listing does not carry answers
        // "Invalid device", so the remedy has to be a different one.
        TargetFacts::PinnedNotIos { udid, name } => Check::new(
            "device",
            CheckStatus::Fail,
            format!(
                "GLASS_IOS_UDID pins {udid} ({name}), which is not an iOS simulator — glass would \
                 attach to it and every iOS call would fail"
            ),
        )
        .with_remedy("pin an iOS simulator's udid, or unset GLASS_IOS_UDID and let glass pick"),
        TargetFacts::PinnedMissing(udid) => Check::new(
            "device",
            CheckStatus::Fail,
            format!("GLASS_IOS_UDID pins {udid}, which this host has no simulator for"),
        )
        .with_remedy(
            "check the udid against `xcrun simctl list devices available`, or unset GLASS_IOS_UDID",
        ),
        // Two different failures share `Resolve::Error`: the host has nothing, or the operator's
        // `GLASS_IOS_DEVICE` matches nothing it has. Telling the second one to create a simulator
        // hides the variable that actually decided it.
        TargetFacts::Unresolvable { why, named } => Check::new(
            "device",
            CheckStatus::Fail,
            why.clone(),
        )
        .with_remedy(if *named {
            "correct GLASS_IOS_DEVICE to a simulator this host has, or unset it and let glass \
                 pick"
        } else {
            "create one in Xcode (Window > Devices and Simulators) or with `xcrun simctl create`"
        }),
        TargetFacts::Unknown(cause) => Check::new(
            "device",
            CheckStatus::Warn,
            format!("could not tell what glass would drive: {cause}"),
        )
        .with_remedy("run `xcrun simctl list devices available --json` and check it parses"),
    }
}

/// Budget for each doctor probe. Doctor reports on the host rather than driving it, so every
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
    let target = gather_target(&simctl_out, &|k| std::env::var(k).ok());

    build_checks(&Probe {
        xcode_dir,
        simctl_ok,
        runtimes: &runtimes,
        target: &target,
    })
}

/// Outcome of the `--deep` `idb_companion` health probe (see [`probe_companion`]).
/// `Started`/`FailedToStart` come from a real spawn against an already-booted simulator;
/// `SelfTestOk`/`SelfTestFailed` from the bounded `--version` fallback used when none is booted.
/// A binary that never resolved is not among them — [`companion_check`] answers that from the
/// resolution, without spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CompanionProbe {
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

/// The companion's bounded `--version` self-test: does the binary actually execute? Used only
/// when no simulator is booted. Captures stderr to a temp file — a file can't fill and block,
/// mirroring `IdbCompanion` — and surfaces it as the cause on failure.
fn self_test_with(bin: &Path) -> CompanionProbe {
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
            return CompanionProbe::SelfTestFailed(format!(
                "spawn {} {SELF_TEST_ARG}: {e}",
                bin.display()
            ));
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

/// Whether this host has an `idb_companion` glass can actually run — the gate on the iOS
/// input and accessibility capabilities.
pub(crate) fn companion_runnable() -> bool {
    matches!(gather().resolved, Resolved::Found(_))
}

/// What this host has, read from the real environment.
fn gather() -> CompanionFacts {
    CompanionFacts::gather(&|k| std::env::var(k).ok())
}

/// The `idb_companion` line for `glass doctor`, and — with `deep` — the health probe behind it.
///
/// `deep` costs a real companion start, which is why the aggregator gates it on iOS being the
/// selected backend while emitting the line either way.
pub fn companion_check(deep: bool) -> Check {
    check_for(&gather(), deep, probe_companion)
}

/// [`companion_check`] over gathered facts, with the probe injected — the seam that pins which
/// resolutions are worth a real companion start without one running here.
fn check_for(
    facts: &CompanionFacts,
    deep: bool,
    probe: impl FnOnce(&Path) -> CompanionProbe,
) -> Check {
    match &facts.resolved {
        // Only a runnable binary is worth starting: probing any other outcome would report a
        // spawn failure whose cause is the resolution the check already has in hand.
        Resolved::Found(bin) if deep => deep_check(&probe(bin)),
        _ => resolution_check(facts),
    }
}

/// The shared `idb_companion` install remedy.
const INSTALL_REMEDY: &str =
    "brew tap facebook/fb && brew trust facebook/fb && brew install idb-companion";

/// Pure: what a resolution means for the operator.
///
/// A companion glass cannot run is a **Fail**, not a Warn: unlike android, which keeps barebones
/// function without its companions, iOS cannot drive apps at all without this one. The aggregator
/// softens that to a Warn when iOS is not the selected backend.
fn resolution_check(facts: &CompanionFacts) -> Check {
    match &facts.resolved {
        Resolved::Found(p) => Check::new(
            "idb_companion",
            CheckStatus::Ok,
            format!("{} — input + accessibility are available", p.display()),
        ),
        // It is installed; what it needs is permission, not another `brew install`.
        Resolved::NotExecutable(p) => Check::new(
            "idb_companion",
            CheckStatus::Fail,
            format!(
                "{} — not executable; input + accessibility are unavailable (iOS cannot drive apps)",
                p.display()
            ),
        )
        .with_remedy(format!(
            "chmod +x {}, or point GLASS_IDB_COMPANION at a runnable binary",
            p.display()
        )),
        // An override skips discovery, so the variable — not what this host has installed —
        // is what left glass with nothing.
        Resolved::Absent if facts.override_set => Check::new(
            "idb_companion",
            CheckStatus::Fail,
            "GLASS_IDB_COMPANION does not name a runnable idb_companion — input + accessibility \
             are unavailable (iOS cannot drive apps)",
        )
        .with_remedy(
            "point GLASS_IDB_COMPANION at a runnable idb_companion, or unset it to search PATH \
             and Homebrew's standard prefixes",
        ),
        Resolved::Absent => not_found_check(),
        // Nothing was looked up, so "not found" would be a claim this check never established
        // (glass#373).
        Resolved::NoSearchPath => Check::new(
            "idb_companion",
            CheckStatus::Fail,
            "idb_companion could not be looked up — PATH is unset in glass's environment",
        )
        .with_remedy(format!(
            "set GLASS_IDB_COMPANION to its absolute path, give glass a PATH to search, or \
             install it: {INSTALL_REMEDY}"
        )),
    }
}

/// The Fail for a companion that is nowhere to be found.
fn not_found_check() -> Check {
    Check::new(
        "idb_companion",
        CheckStatus::Fail,
        "idb_companion not found — input + accessibility are unavailable (iOS cannot drive apps)",
    )
    .with_remedy(INSTALL_REMEDY)
}

/// Pure: what a `--deep` probe proved. Broken (`FailedToStart`/`SelfTestFailed`) ⇒ Fail: iOS
/// cannot drive apps without the companion. Unverified (`SelfTestOk` — the binary runs but no
/// booted simulator was available to exercise a real start) ⇒ Warn.
fn deep_check(probe: &CompanionProbe) -> Check {
    match probe {
        CompanionProbe::Started => Check::new(
            "idb_companion",
            CheckStatus::Ok,
            "started and served its gRPC socket — input + accessibility are available",
        ),
        CompanionProbe::SelfTestOk => Check::new(
            "idb_companion",
            CheckStatus::Warn,
            "binary runs, but no booted simulator was available to verify a real start — \
             boot one and re-run with --deep to exercise the companion",
        ),
        CompanionProbe::FailedToStart(cause) => Check::new(
            "idb_companion",
            CheckStatus::Fail,
            format!(
                "failed to start: {cause} — input + accessibility are unavailable (iOS is observe-only)"
            ),
        )
        .with_remedy(INSTALL_REMEDY),
        CompanionProbe::SelfTestFailed(cause) => Check::new(
            "idb_companion",
            CheckStatus::Fail,
            format!("binary failed to execute: {cause} — input + accessibility are unavailable"),
        )
        .with_remedy(INSTALL_REMEDY),
    }
}

/// The `--deep` companion health probe: does `bin` actually start? Reuses the real runtime spawn
/// path against an *already-booted* simulator — never booting one, so it stays bounded (the spawn
/// carries its own socket deadline) and non-mutating. With no simulator booted, falls back to the
/// bounded [`self_test_with`] so `--deep` still yields a signal.
fn probe_companion(bin: &Path) -> CompanionProbe {
    match booted_udid() {
        Some(udid) => match IdbCompanion::spawn_bin(&udid, bin) {
            // Dropping the companion kills+reaps the child and removes its socket.
            Ok(companion) => {
                drop(companion);
                CompanionProbe::Started
            }
            // The error already embeds the companion's captured stderr; strip the redundant
            // `GlassError::Backend` Display prefix (the mapping frames it "failed to start: …").
            Err(e) => CompanionProbe::FailedToStart(spawn_cause(e)),
        },
        None => self_test_with(bin),
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
/// One listing, not two: a second `simctl` call is a second thing that can fail, and a failed
/// listing has to be described rather than read as an empty host.
fn gather_target(
    simctl_out: &dyn Fn(&[&str]) -> Option<String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> TargetFacts {
    let args = ["simctl", "list", "devices", "available", "--json"];
    let Some(json) = simctl_out(&args) else {
        return TargetFacts::Unknown(format!("`xcrun {}` did not answer", args.join(" ")));
    };
    match parse_devices(&json) {
        Ok(devices) => {
            let (udid, name, _) = wants(env);
            target_from(&devices, udid.as_deref(), name.as_deref())
        }
        Err(e) => TargetFacts::Unknown(e.to_string()),
    }
}

/// The pure half of [`gather_target`]: run the start path's own resolution and describe its outcome.
///
/// Both preferences are passed because both change what a driving call does — `GLASS_IOS_DEVICE`
/// naming a simulator this host lacks makes `glass_start` fail outright.
fn target_from(
    devices: &[SimDevice],
    want_udid: Option<&str>,
    want_name: Option<&str>,
) -> TargetFacts {
    let available = devices.iter().filter(|d| ios_target(d)).count();
    match resolve(devices, want_udid, want_name) {
        Resolve::Attach(udid) => match devices.iter().find(|d| d.udid == udid) {
            // Only a pinned udid can name something the listing does not carry: the unpinned path
            // picks from the listing itself.
            None => TargetFacts::PinnedMissing(udid),
            Some(d) if d.state != "Booted" => TargetFacts::PinnedNotBooted(udid),
            Some(d) if !d.runtime.contains("iOS") => TargetFacts::PinnedNotIos {
                udid,
                name: d.name.clone(),
            },
            Some(d) => TargetFacts::Attaching {
                name: d.name.clone(),
                available,
            },
        },
        Resolve::Boot(udid) => TargetFacts::WillBoot {
            name: devices
                .iter()
                .find(|d| d.udid == udid)
                .map_or(udid, |d| d.name.clone()),
            available,
        },
        Resolve::Error(why) => TargetFacts::Unresolvable {
            why,
            named: want_name.is_some(),
        },
    }
}

/// A device a driving call could use: on an iOS runtime (a watchOS or tvOS simulator is never a
/// boot candidate), and either offered by the listing or already booted.
///
/// The booted half matters: `resolve` attaches to a booted device without consulting `is_available`,
/// so counting only available ones let the line read "0 iOS simulator(s) available, booted: X".
fn ios_target(d: &SimDevice) -> bool {
    d.runtime.contains("iOS") && (d.is_available || d.state == "Booted")
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
    use std::path::PathBuf;

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

    fn device_line(target: &TargetFacts) -> Check {
        let runtimes = vec!["iOS 26.5".to_string()];
        let p = Probe {
            xcode_dir: Some("/Applications/Xcode.app/Contents/Developer".into()),
            simctl_ok: true,
            runtimes: &runtimes,
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
        let c = device_line(&TargetFacts::WillBoot {
            name: "iPhone 17".into(),
            available: 5,
        });
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(
            c.detail.contains("boots iPhone 17 at start"),
            "{}",
            c.detail
        );
    }

    #[test]
    fn a_pinned_udid_that_is_not_booted_is_the_one_state_start_will_not_fix() {
        // `resolve` returns a pinned udid without checking its state and without booting it, so
        // glass attaches to a dead target and every later call fails against it.
        let c = device_line(&TargetFacts::PinnedNotBooted("DEAD-UDID".into()));
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.remedy
                .as_deref()
                .unwrap()
                .contains("simctl boot DEAD-UDID"),
            "{:?}",
            c.remedy
        );
    }

    #[test]
    fn a_pinned_udid_the_host_lacks_is_not_told_to_boot_it() {
        // `simctl boot` on an unknown udid answers "Invalid device", so this needs its own remedy.
        let c = device_line(&TargetFacts::PinnedMissing("GHOST".into()));
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            !c.remedy.as_deref().unwrap().contains("simctl boot"),
            "{:?}",
            c.remedy
        );
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
    fn the_reported_device_is_the_one_a_driving_call_would_attach_to() {
        // Two booted devices and a booted watch: a naive "first booted" pick names the watch or the
        // wrong simulator, neither of which is what `resolve` — and so a driving call — attaches to.
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
        assert_eq!(
            target_from(&devices, None, None),
            TargetFacts::Attaching {
                name: "iPhone 17 Pro".into(),
                available: 2,
            }
        );
    }

    #[test]
    fn an_ipad_only_host_is_driveable_and_reported_so() {
        // `resolve` drives any iOS-family device and only *prefers* an iPhone, so failing an
        // iPad-only host with "no iPhone simulator available" would report a working host as broken.
        let devices = vec![dev("IPAD", "iPad Pro 13-inch", "Shutdown")];
        assert_eq!(
            target_from(&devices, None, None),
            TargetFacts::WillBoot {
                name: "iPad Pro 13-inch".into(),
                available: 1,
            }
        );
    }

    #[test]
    fn a_pinned_udid_reaches_the_resolution_from_the_environment() {
        // Proven missing by mutation: dropping the udid on the way through `gather_target` left the
        // whole suite green while the doctor reported a green line for a host whose pinned target is
        // a shut-down device — the dead-target case this check exists for.
        let json = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-5":[
            {"udid":"AAA","name":"iPhone 17","state":"Shutdown","isAvailable":true},
            {"udid":"BBB","name":"iPhone 17 Pro","state":"Booted","isAvailable":true}]}}"#;
        let facts = gather_target(&|_| Some(json.to_string()), &|k| {
            (k == "GLASS_IOS_UDID").then(|| "AAA".to_string())
        });
        assert_eq!(facts, TargetFacts::PinnedNotBooted("AAA".into()));
    }

    #[test]
    fn a_name_preference_that_matches_nothing_is_remedied_by_fixing_the_variable() {
        // Not "create a simulator in Xcode": this host has one, and the operator's own variable is
        // what ruled it out.
        let c = device_line(&TargetFacts::Unresolvable {
            why: "no available simulator named \"iPhone 16\"".into(),
            named: true,
        });
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.remedy.as_deref().unwrap().contains("GLASS_IOS_DEVICE"),
            "{:?}",
            c.remedy
        );
    }

    #[test]
    fn a_pinned_udid_on_a_non_ios_runtime_is_caught_before_it_is_driven() {
        // `resolve` returns a pinned udid whatever its runtime, so glass would attach to a watch and
        // every iOS call would fail against it.
        let devices = vec![dev_on(
            "WATCH",
            "Apple Watch Series 10",
            "Booted",
            "com.apple.CoreSimulator.SimRuntime.watchOS-10-4",
        )];
        assert_eq!(
            target_from(&devices, Some("WATCH"), None),
            TargetFacts::PinnedNotIos {
                udid: "WATCH".into(),
                name: "Apple Watch Series 10".into(),
            }
        );
        assert_eq!(
            device_line(&target_from(&devices, Some("WATCH"), None)).status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn the_count_never_contradicts_the_device_it_names() {
        // `resolve` attaches to a booted device without consulting `is_available`, so a listing that
        // omits the availability key used to render "0 iOS simulator(s) available, booted: X".
        let devices = vec![SimDevice {
            udid: "AAA".into(),
            name: "iPhone 17".into(),
            state: "Booted".into(),
            runtime: "com.apple.CoreSimulator.SimRuntime.iOS-26-5".into(),
            is_available: false,
        }];
        assert_eq!(
            target_from(&devices, None, None),
            TargetFacts::Attaching {
                name: "iPhone 17".into(),
                available: 1,
            }
        );
    }

    #[test]
    fn a_pin_is_missing_when_the_listing_does_not_carry_it() {
        let devices = vec![dev("BBB", "iPhone 17 Pro", "Booted")];
        assert_eq!(
            target_from(&devices, Some("TYPO"), None),
            TargetFacts::PinnedMissing("TYPO".into())
        );
        assert_eq!(
            target_from(&devices, Some("BBB"), None),
            TargetFacts::Attaching {
                name: "iPhone 17 Pro".into(),
                available: 1,
            }
        );
    }

    #[test]
    fn a_device_name_that_matches_nothing_is_reported_not_ignored() {
        // `GLASS_IOS_DEVICE` changes what a driving call resolves: a name this host lacks makes
        // `glass_start` fail outright, so reading only the udid preference would report a green
        // line about a device glass would never attach to.
        let devices = vec![dev("AAA", "iPhone 17", "Shutdown")];
        let facts = target_from(&devices, None, Some("iPhone 16"));
        assert!(
            matches!(&facts, TargetFacts::Unresolvable { why, named: true } if why.contains("iPhone 16")),
            "{facts:?}"
        );
    }

    #[test]
    fn a_listing_that_does_not_answer_names_the_command_that_did_not_answer() {
        let facts = gather_target(&|_| None, &|_| None);
        match facts {
            TargetFacts::Unknown(cause) => {
                assert!(
                    cause.contains("simctl list devices available --json"),
                    "{cause}"
                )
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_read_listing_carries_both_preferences_from_the_environment() {
        // Shut down, deliberately: `resolve` answers from the booted devices before it consults
        // `GLASS_IOS_DEVICE`, so the name preference only changes the outcome when nothing is
        // booted — and a fixture with a booted device would pass whether or not the doctor read
        // the variable at all.
        let json = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-5":[
            {"udid":"BBB","name":"iPhone 17 Pro","state":"Shutdown","isAvailable":true}]}}"#;
        let facts = gather_target(&|_| Some(json.to_string()), &|k| {
            (k == "GLASS_IOS_DEVICE").then(|| "iPhone 16".to_string())
        });
        assert!(
            matches!(&facts, TargetFacts::Unresolvable { why, named: true } if why.contains("iPhone 16")),
            "{facts:?}"
        );
    }

    #[test]
    fn a_booted_device_wins_over_a_name_preference_that_matches_nothing() {
        // Measured from `resolve`: the booted branch ignores `want_name` entirely. Pinned here so
        // the doctor's "what would a driving call do" claim stays true if that ordering changes.
        let devices = vec![dev("BBB", "iPhone 17 Pro", "Booted")];
        assert_eq!(
            target_from(&devices, None, Some("iPhone 16")),
            TargetFacts::Attaching {
                name: "iPhone 17 Pro".into(),
                available: 1,
            }
        );
    }

    #[test]
    fn all_green_when_fully_configured() {
        let runtimes = vec!["iOS 26.5".to_string()];
        let p = Probe {
            xcode_dir: Some("/Applications/Xcode.app/Contents/Developer".into()),
            simctl_ok: true,
            runtimes: &runtimes,
            target: &TargetFacts::Attaching {
                name: "iPhone 17".into(),
                available: 1,
            },
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
            target: &TargetFacts::Unresolvable {
                why: "no available iPhone simulator found".into(),
                named: false,
            },
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
            target: &TargetFacts::Unresolvable {
                why: "no available iPhone simulator found".into(),
                named: false,
            },
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
            target: &TargetFacts::Unresolvable {
                why: "no available iPhone simulator found".into(),
                named: false,
            },
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

    /// Gathered facts for a resolution that discovery reached (no `GLASS_IDB_COMPANION`).
    fn discovered(resolved: Resolved) -> CompanionFacts {
        CompanionFacts {
            resolved,
            override_set: false,
        }
    }

    #[test]
    fn a_runnable_companion_reads_ok_and_names_the_binary() {
        let c = resolution_check(&discovered(Resolved::Found(PathBuf::from(
            "/opt/homebrew/bin/idb_companion",
        ))));
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(
            c.detail.contains("/opt/homebrew/bin/idb_companion"),
            "the operator needs to know which binary answered: {}",
            c.detail
        );
        assert_eq!(c.remedy, None);
    }

    /// glass#393: an `idb_companion` that is installed and unrunnable used to read as missing,
    /// sending the user to reinstall a package that is already there. It has to name the file
    /// and the permission fix instead.
    #[test]
    fn a_companion_that_cannot_be_executed_is_told_apart_from_a_missing_one() {
        let c = resolution_check(&discovered(Resolved::NotExecutable(PathBuf::from(
            "/usr/local/bin/idb_companion",
        ))));
        assert_eq!(c.status, CheckStatus::Fail);
        let remedy = c
            .remedy
            .as_deref()
            .expect("an unrunnable binary has a remedy");
        assert!(
            remedy.contains("chmod +x /usr/local/bin/idb_companion"),
            "remedy must name the permission fix on the real path: {remedy}"
        );
        assert_ne!(
            remedy, INSTALL_REMEDY,
            "the companion is installed — reinstalling it changes nothing"
        );
    }

    #[test]
    fn a_companion_that_is_nowhere_points_at_the_install() {
        let c = resolution_check(&discovered(Resolved::Absent));
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.remedy.as_deref(), Some(INSTALL_REMEDY));
    }

    /// glass#373: MCP clients routinely spawn glass-mcp with a stripped environment. "Not found"
    /// there is a claim the check never established.
    #[test]
    fn a_companion_that_could_not_be_looked_up_says_so_rather_than_not_found() {
        let c = resolution_check(&discovered(Resolved::NoSearchPath));
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.detail.contains("PATH"),
            "detail must name the unset PATH: {}",
            c.detail
        );
        let remedy = c
            .remedy
            .as_deref()
            .expect("a stripped environment has a remedy");
        assert!(
            remedy.contains("GLASS_IDB_COMPANION"),
            "remedy must name the override that needs no PATH: {remedy}"
        );
    }

    /// An override skips discovery, so "not found — brew install" would prescribe a fix that
    /// changes nothing: glass would still read only the variable.
    #[test]
    fn an_override_that_names_nothing_blames_the_variable_not_the_install() {
        let c = resolution_check(&CompanionFacts {
            resolved: Resolved::Absent,
            override_set: true,
        });
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.detail.contains("GLASS_IDB_COMPANION"),
            "detail must name the variable that decided this: {}",
            c.detail
        );
        assert_ne!(
            c.remedy.as_deref(),
            Some(INSTALL_REMEDY),
            "installing another copy would not be looked at while the override stands"
        );
    }

    #[test]
    fn a_deep_check_probes_the_binary_the_resolution_found() {
        let probed = std::cell::Cell::new(String::new());
        let c = check_for(
            &discovered(Resolved::Found(PathBuf::from(
                "/opt/homebrew/bin/idb_companion",
            ))),
            true,
            |bin| {
                probed.set(bin.display().to_string());
                CompanionProbe::Started
            },
        );
        assert_eq!(
            probed.take(),
            "/opt/homebrew/bin/idb_companion",
            "the probe must start the binary the check reported, not resolve a second time"
        );
        assert_eq!(c.status, CheckStatus::Ok);
    }

    /// A binary glass cannot execute needs no spawn to prove it: probing would report a
    /// permission failure as if the companion were broken, burying the fix.
    #[test]
    fn a_deep_check_never_probes_a_binary_it_already_knows_it_cannot_run() {
        let c = check_for(
            &discovered(Resolved::NotExecutable(PathBuf::from(
                "/usr/local/bin/idb_companion",
            ))),
            true,
            |_| panic!("an unrunnable binary must not be spawned"),
        );
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.remedy
                .as_deref()
                .is_some_and(|r| r.contains("chmod +x /usr/local/bin/idb_companion")),
            "remedy must still be the permission fix: {:?}",
            c.remedy
        );
    }

    #[test]
    fn a_shallow_check_never_probes_at_all() {
        let c = check_for(
            &discovered(Resolved::Found(PathBuf::from(
                "/opt/homebrew/bin/idb_companion",
            ))),
            false,
            |_| panic!("a check without --deep must not spawn the companion"),
        );
        assert_eq!(c.status, CheckStatus::Ok);
    }

    #[test]
    fn deep_check_maps_every_probe_outcome() {
        let started = deep_check(&CompanionProbe::Started);
        assert_eq!(started.status, CheckStatus::Ok);
        assert_eq!(started.remedy, None);

        let unverified = deep_check(&CompanionProbe::SelfTestOk);
        assert_eq!(unverified.status, CheckStatus::Warn);

        let broken = deep_check(&CompanionProbe::FailedToStart("exited 1: boom".into()));
        assert_eq!(broken.status, CheckStatus::Fail);
        assert!(
            broken.detail.contains("boom"),
            "cause must surface: {}",
            broken.detail
        );
        assert_eq!(broken.remedy.as_deref(), Some(INSTALL_REMEDY));

        let unrunnable = deep_check(&CompanionProbe::SelfTestFailed("spawn: nope".into()));
        assert_eq!(unrunnable.status, CheckStatus::Fail);
        assert!(
            unrunnable.detail.contains("nope"),
            "cause must surface: {}",
            unrunnable.detail
        );
    }

    #[test]
    fn self_test_ok_when_binary_exits_zero() {
        // `/bin/echo --version` prints and exits 0 regardless of args — stands in for a
        // healthy idb_companion whose real `--version` also exits 0.
        assert_eq!(
            self_test_with(Path::new("/bin/echo")),
            CompanionProbe::SelfTestOk
        );
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
        let bin = script.as_path();

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
        match self_test_with(Path::new("/nonexistent/definitely-not-a-binary")) {
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
