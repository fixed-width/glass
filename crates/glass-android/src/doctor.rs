//! `glass doctor` checks for the Android backend: are adb, the emulator, an AVD,
//! and an online device present (and, with `--deep`, can we capture + dump a11y)?
//!
//! Pure `build_checks(&Probe)` over observed state, plus a thin subprocess `probe`.
//! Reports `Check` statuses; never errors.

use glass_core::{Check, CheckStatus};

use crate::adb::Adb;
use crate::avd::{decide, parse_list_avds, resolve_emulator_bin, Action, Lifecycle};
use crate::axmap::check_dump_status;
use crate::target::{parse_devices, Device};

/// Observed host state for the Android doctor checks. Captured by `probe`, consumed
/// by the pure `build_checks` so all branch logic is unit-testable without subprocesses.
struct Probe {
    /// Resolved adb path (`GLASS_ADB` / discovered SDK / `"adb"` on PATH).
    adb_bin: String,
    /// Human description of how adb was resolved (path + source), for the OK detail.
    adb_detail: String,
    /// Ordered candidates discovery considered (env vars + default locations), for the
    /// Fail trail.
    adb_trail: Vec<String>,
    /// First line of `adb version`; `None` when adb is absent/unrunnable.
    adb_version: Option<String>,
    /// Resolved emulator path (`GLASS_EMULATOR`/SDK root/`"emulator"`).
    emulator_bin: String,
    /// AVDs from `emulator -list-avds`; `None` when the binary is absent/failed,
    /// `Some(vec![])` when it ran but found none.
    avds: Option<Vec<String>>,
    /// Serials with `adb devices` state `"device"` (online), for display.
    online: Vec<String>,
    /// What `glass_start` would do with these devices — the runtime's own
    /// `decide(online, GLASS_ANDROID_SERIAL, GLASS_ANDROID_LIFECYCLE)` verdict, so the
    /// doctor matches the real backend (attach a serial / boot / refuse) by construction.
    selection: Action,
    /// Whether `--deep` was requested (so `build_checks` can pick the right Skip reason).
    deep_requested: bool,
    /// Deep-probe results; `Some` only when `deep_requested` && adb present && a device is online.
    deep: Option<DeepProbe>,
    /// `GLASS_ANDROID_AGENT` is explicitly `off`.
    agent_off: bool,
    /// Resolved `GLASS_ANDROID_AGENT_JAR` (non-empty), else `None`.
    agent_jar: Option<String>,
    /// The `agent_jar` path is an existing file (false when `agent_jar` is `None`).
    agent_jar_exists: bool,
    /// Deep launch+ping result; `Some` only when deep && configured && a device is attachable.
    agent_deep: Option<Result<(), String>>,
    /// `GLASS_ANDROID_A11Y` is explicitly `off`.
    a11y_off: bool,
    /// Resolved `GLASS_ANDROID_A11Y_APK` (non-empty), else `None`.
    a11y_apk: Option<String>,
    /// The `a11y_apk` path is an existing file (false when `a11y_apk` is `None`).
    a11y_apk_exists: bool,
    /// Deep install+enable+ping result; `Some` only when deep && configured && a device is attachable.
    a11y_deep: Option<Result<(), String>>,
}

/// Result of the deep probes against one online device. `Ok` carries a human detail,
/// `Err` a failure reason.
struct DeepProbe {
    serial: String,
    screencap: Result<String, String>,
    uiautomator: Result<String, String>,
}

/// Build the Android doctor checks by probing the host. `deep` additionally captures a
/// frame and an a11y dump from an already-online device (it never boots one).
pub fn checks(deep: bool) -> Vec<Check> {
    build_checks(&probe(deep))
}

fn probe(deep_requested: bool) -> Probe {
    let get = |k: &str| std::env::var(k).ok();
    let adb_resolution = crate::sdk::resolve_adb(&get, &|p| p.exists());
    let adb_detail = adb_resolution.describe();
    let adb_trail = crate::sdk::sdk_search_trail(&get);
    let adb = Adb::from_env();
    let adb_bin = adb.bin().to_string();
    let adb_version = adb.run(["version"]).ok().map(|s| first_line(&s)).filter(|s| !s.is_empty());
    let emulator_bin = resolve_emulator_bin(&get, &|p| p.exists());
    let avds = list_avds(&emulator_bin);

    let online_devices: Vec<Device> = if adb_version.is_some() {
        parse_devices(&adb.run(["devices"]).unwrap_or_default())
            .into_iter()
            .filter(|d| d.state == "device")
            .collect()
    } else {
        Vec::new()
    };
    let online: Vec<String> = online_devices.iter().map(|d| d.serial.clone()).collect();
    // Reuse the runtime's own selection policy so the doctor's verdict matches what
    // `glass_start` will actually do (attach a specific serial / boot / refuse).
    let lifecycle = Lifecycle::from_env(get("GLASS_ANDROID_LIFECYCLE").as_deref());
    let selection = decide(&online_devices, get("GLASS_ANDROID_SERIAL").as_deref(), lifecycle);

    // Deep-probe exactly the device glass would attach to (it never boots one).
    let deep = match &selection {
        Action::Attach(serial) if deep_requested => Some(deep_probe(&adb, serial)),
        _ => None,
    };

    let agent_off = get("GLASS_ANDROID_AGENT").map(|v| v.eq_ignore_ascii_case("off")).unwrap_or(false);
    let agent_jar = crate::agent::agent_jar(&get);
    let agent_jar_exists =
        agent_jar.as_deref().map(|p| std::path::Path::new(p).is_file()).unwrap_or(false);
    // Deep agent probe: launch the agent on the device glass would attach to, ping it, then
    // tear it down (reuses the production lifecycle; leak-free — no pkill).
    let agent_deep = match &selection {
        Action::Attach(serial) if deep_requested && !agent_off && agent_jar_exists => {
            let reg = crate::agent::AgentRegistry::new();
            let r = reg.ensure(&adb.with_serial(serial.clone())).map(|_| ()).map_err(|e| e.to_string());
            reg.shutdown();
            Some(r)
        }
        _ => None,
    };

    let a11y_off = get("GLASS_ANDROID_A11Y").map(|v| v.eq_ignore_ascii_case("off")).unwrap_or(false);
    let a11y_apk = crate::a11y_service::a11y_apk(&get);
    let a11y_apk_exists =
        a11y_apk.as_deref().map(|p| std::path::Path::new(p).is_file()).unwrap_or(false);
    // Deep a11y probe: install + enable the service on the device glass would attach to,
    // ping it, then tear it down (reuses the production lifecycle; idempotent + clean).
    let a11y_deep = match &selection {
        Action::Attach(serial) if deep_requested && !a11y_off && a11y_apk_exists => {
            let apk = a11y_apk.as_deref().unwrap();
            let reg = crate::a11y_service::A11yServiceRegistry::new();
            let r = reg.ensure(&adb.with_serial(serial.clone()), apk).map(|_| ()).map_err(|e| e.to_string());
            reg.shutdown();
            Some(r)
        }
        _ => None,
    };

    Probe {
        adb_bin,
        adb_detail,
        adb_trail,
        adb_version,
        emulator_bin,
        avds,
        online,
        selection,
        deep_requested,
        deep,
        agent_off,
        agent_jar,
        agent_jar_exists,
        agent_deep,
        a11y_off,
        a11y_apk,
        a11y_apk_exists,
        a11y_deep,
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// `<bin> -list-avds`: `None` on spawn failure (binary absent), else the parsed names.
fn list_avds(bin: &str) -> Option<Vec<String>> {
    std::process::Command::new(bin)
        .arg("-list-avds")
        .output()
        .ok()
        .map(|o| parse_list_avds(&String::from_utf8_lossy(&o.stdout)))
}

/// Deep probe one online device: capture a frame (validated via the real decoder) and
/// an a11y dump (via `uiautomator dump`, mirroring `AndroidA11y`).
fn deep_probe(adb: &Adb, serial: &str) -> DeepProbe {
    const DUMP_PATH: &str = "/sdcard/glass_doctor_dump.xml";
    let dev = adb.with_serial(serial);

    let screencap = match dev.run_bytes(["exec-out", "screencap"]) {
        Ok(bytes) => match crate::screencap::decode_screencap(&bytes) {
            Ok(f) => Ok(format!("captured {}x{}, {} bytes raw", f.width, f.height, bytes.len())),
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e.to_string()),
    };

    // Remove any stale dump so a failed `uiautomator dump` can't yield a false positive
    // from a prior run's file; validate the dump command's own status (parity with
    // AndroidA11y) before trusting the cat'd XML.
    let _ = dev.run(["shell", "rm", "-f", DUMP_PATH]);
    let uiautomator = match dev
        .run(["shell", "uiautomator", "dump", DUMP_PATH])
        .and_then(|status| {
            check_dump_status(&status)?;
            dev.run(["shell", "cat", DUMP_PATH])
        }) {
        Ok(xml) if xml.contains("<hierarchy") => Ok("a11y dump OK".to_string()),
        Ok(_) => Err("uiautomator dump produced no hierarchy".to_string()),
        Err(e) => Err(e.to_string()),
    };

    DeepProbe { serial: serial.to_string(), screencap, uiautomator }
}

/// Build the Android doctor section's checks from observed state. Pure.
fn build_checks(p: &Probe) -> Vec<Check> {
    let (screencap, uiautomator) = deep_checks(p);
    vec![adb_check(p), emulator_check(p), device_check(p), agent_check(p), a11y_check(p), screencap, uiautomator]
}

fn agent_check(p: &Probe) -> Check {
    if p.agent_off {
        return Check::new("agent", CheckStatus::Skip, "disabled (GLASS_ANDROID_AGENT=off)");
    }
    let Some(jar) = &p.agent_jar else {
        return Check::new(
            "agent",
            CheckStatus::Skip,
            "not configured (optional — set GLASS_ANDROID_AGENT_JAR for clipboard + high-fidelity input)",
        );
    };
    if !p.agent_jar_exists {
        return Check::new(
            "agent",
            CheckStatus::Warn,
            format!("GLASS_ANDROID_AGENT_JAR={jar} but no file there"),
        )
        .with_remedy("build the agent (`./gradlew dex` in glass-android-agent), or fix the path");
    }
    // agent_deep is Some only when deep was requested (see probe); so None means
    // either not-deep (Ok configured) or deep-but-no-attachable-device (Skip).
    match (&p.agent_deep, p.deep_requested) {
        (Some(Ok(())), _) => {
            Check::new("agent", CheckStatus::Ok, format!("reachable (launched + ping ok): {jar}"))
        }
        (Some(Err(e)), _) => {
            Check::new("agent", CheckStatus::Fail, format!("agent did not come up: {e}"))
                .with_remedy("ensure the device allows `app_process` and the jar is a valid dexed build")
        }
        (None, false) => Check::new("agent", CheckStatus::Ok, format!("configured: {jar}")),
        (None, true) => {
            // agent_deep is Some only when deep + the device is Attach-able, so here the
            // selection is Boot (no device yet) or Error (devices present but refused).
            let reason = match &p.selection {
                Action::Boot => "no online device to probe (glass will boot one on start)",
                _ => "no attachable device to probe (see the device check)",
            };
            Check::new("agent", CheckStatus::Skip, reason)
        }
    }
}

fn a11y_check(p: &Probe) -> Check {
    if p.a11y_off {
        return Check::new("a11y-service", CheckStatus::Skip, "disabled (GLASS_ANDROID_A11Y=off)");
    }
    let Some(apk) = &p.a11y_apk else {
        return Check::new(
            "a11y-service",
            CheckStatus::Skip,
            "not configured (optional — set GLASS_ANDROID_A11Y_APK for a Compose-rich tree + high-fidelity set_value)",
        );
    };
    if !p.a11y_apk_exists {
        return Check::new(
            "a11y-service",
            CheckStatus::Warn,
            format!("GLASS_ANDROID_A11Y_APK={apk} but no file there"),
        )
        .with_remedy("build the APK (`./gradlew :a11y:assembleDebug` in glass-android-agent), or fix the path");
    }
    // a11y_deep is Some only when deep was requested (see probe); so None means
    // either not-deep (Ok configured) or deep-but-no-attachable-device (Skip).
    match (&p.a11y_deep, p.deep_requested) {
        (Some(Ok(())), _) => {
            Check::new("a11y-service", CheckStatus::Ok, format!("reachable (installed + enabled + ping ok): {apk}"))
        }
        (Some(Err(e)), _) => {
            Check::new("a11y-service", CheckStatus::Fail, format!("service did not come up: {e}"))
                .with_remedy("ensure the device allows enabling an AccessibilityService and the APK is a valid debug build")
        }
        (None, false) => Check::new("a11y-service", CheckStatus::Ok, format!("configured: {apk}")),
        (None, true) => Check::new("a11y-service", CheckStatus::Skip, "no online device to probe"),
    }
}

fn deep_checks(p: &Probe) -> (Check, Check) {
    let skip = |reason: &str| {
        (
            Check::new("screencap", CheckStatus::Skip, reason.to_string()),
            Check::new("uiautomator", CheckStatus::Skip, reason.to_string()),
        )
    };
    // Distinguish the three "nothing to probe" cases: adb first, then the missing flag,
    // then no selected device (post-guards, `deep.is_none()` ⟺ no device glass would
    // attach to — none online, or online-but-ambiguous without GLASS_ANDROID_SERIAL).
    if p.adb_version.is_none() {
        return skip("skipped — adb unavailable");
    }
    if !p.deep_requested {
        return skip("run with --deep to probe capture");
    }
    let Some(d) = &p.deep else {
        return skip("no device selected to probe");
    };
    let render = |name: &'static str, res: &Result<String, String>| match res {
        Ok(detail) => Check::new(name, CheckStatus::Ok, format!("{}: {detail}", d.serial)),
        Err(e) => Check::new(name, CheckStatus::Fail, format!("{}: {e}", d.serial))
            .with_remedy("ensure the device is fully booted"),
    };
    (render("screencap", &d.screencap), render("uiautomator", &d.uiautomator))
}

fn device_check(p: &Probe) -> Check {
    if p.adb_version.is_none() {
        return Check::new("device", CheckStatus::Skip, "skipped — adb unavailable");
    }
    // Mirror the runtime's `decide`: Ok when glass would attach to a specific device,
    // Warn when it will boot one, Fail (carrying the runtime's own message) when it would
    // refuse — so a green `device` check guarantees `glass_start` won't reject the host.
    match &p.selection {
        Action::Attach(serial) => Check::new(
            "device",
            CheckStatus::Ok,
            format!("{} online; glass will use {serial}", p.online.len()),
        ),
        Action::Boot => {
            if matches!(&p.avds, Some(avds) if !avds.is_empty()) {
                Check::new(
                    "device",
                    CheckStatus::Warn,
                    "none online; glass will boot an AVD on start (auto lifecycle)",
                )
            } else {
                Check::new("device", CheckStatus::Fail, "no online device and no AVD to boot")
                    .with_remedy("start an emulator (`emulator -avd <name>`) or create an AVD")
            }
        }
        Action::Error(msg) => Check::new("device", CheckStatus::Fail, msg.clone()),
    }
}

fn emulator_check(p: &Probe) -> Check {
    match &p.avds {
        None => Check::new(
            "emulator",
            CheckStatus::Warn,
            format!(
                "emulator binary not found ({}); attach still works, but glass can't boot an AVD",
                p.emulator_bin
            ),
        )
        .with_remedy("install the Android emulator at a standard SDK location (auto-found), or set GLASS_EMULATOR / ANDROID_SDK_ROOT"),
        Some(avds) if avds.is_empty() => Check::new(
            "emulator",
            CheckStatus::Warn,
            format!("{}: no AVDs listed; glass can't boot one", p.emulator_bin),
        )
        .with_remedy(
            "create an AVD (e.g. `avdmanager create avd`); if you expected existing AVDs, check the emulator install",
        ),
        Some(avds) => Check::new(
            "emulator",
            CheckStatus::Ok,
            format!("{} ({} AVD(s): {})", p.emulator_bin, avds.len(), avds.join(", ")),
        ),
    }
}

fn adb_check(p: &Probe) -> Check {
    match &p.adb_version {
        Some(v) => Check::new("adb", CheckStatus::Ok, format!("{} ({v})", p.adb_detail)),
        None => Check::new(
            "adb",
            CheckStatus::Fail,
            format!(
                "`adb` not found — looked in: {}; resolved `{}`",
                p.adb_trail.join(", "),
                p.adb_bin
            ),
        )
        .with_remedy(
            "install Android platform-tools at any standard SDK location (auto-found), or set GLASS_ADB",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_probe() -> Probe {
        Probe {
            adb_bin: "/sdk/platform-tools/adb".into(),
            adb_detail: "/sdk/platform-tools/adb (via discovered SDK /home/u/android-sdk)".into(),
            adb_trail: vec![
                "ANDROID_SDK_ROOT".into(),
                "ANDROID_HOME".into(),
                "/home/u/android-sdk".into(),
            ],
            adb_version: Some("Android Debug Bridge version 1.0.41".into()),
            emulator_bin: "/sdk/emulator/emulator".into(),
            avds: Some(vec!["glass".into()]),
            online: vec!["emulator-5554".into()],
            selection: Action::Attach("emulator-5554".into()),
            deep_requested: false,
            deep: None,
            agent_off: false,
            agent_jar: Some("/sdk/glass-agent.jar".into()),
            agent_jar_exists: true,
            agent_deep: None,
            a11y_off: false,
            a11y_apk: Some("/sdk/glass-a11y.apk".into()),
            a11y_apk_exists: true,
            a11y_deep: None,
        }
    }

    fn deep_ok() -> DeepProbe {
        DeepProbe {
            serial: "emulator-5554".into(),
            screencap: Ok("captured 1080x2400, 10368016 bytes raw".into()),
            uiautomator: Ok("a11y dump OK".into()),
        }
    }

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).expect("check present")
    }

    fn dev(serial: &str) -> Device {
        Device { serial: serial.into(), state: "device".into() }
    }

    #[test]
    fn adb_present_is_ok_with_path_and_version() {
        let c = build_checks(&base_probe());
        let adb = find(&c, "adb");
        assert_eq!(adb.status, CheckStatus::Ok);
        assert!(adb.detail.contains("/sdk/platform-tools/adb"));
        assert!(adb.detail.contains("1.0.41"));
        assert!(adb.remedy.is_none());
    }

    #[test]
    fn adb_absent_fails_with_remedy() {
        let mut p = base_probe();
        p.adb_version = None;
        let c = build_checks(&p);
        let adb = find(&c, "adb");
        assert_eq!(adb.status, CheckStatus::Fail);
        assert!(adb.remedy.as_deref().unwrap().contains("GLASS_ADB"));
    }

    #[test]
    fn adb_ok_detail_shows_resolution_source() {
        let c = build_checks(&base_probe());
        let adb = find(&c, "adb");
        assert_eq!(adb.status, CheckStatus::Ok);
        assert!(adb.detail.contains("via discovered SDK"), "got {}", adb.detail);
    }

    #[test]
    fn adb_fail_detail_shows_search_trail() {
        let mut p = base_probe();
        p.adb_version = None;
        let c = build_checks(&p);
        let adb = find(&c, "adb");
        assert_eq!(adb.status, CheckStatus::Fail);
        assert!(
            adb.detail.contains("looked in: ANDROID_SDK_ROOT, ANDROID_HOME"),
            "got {}",
            adb.detail
        );
    }

    #[test]
    fn emulator_with_avds_is_ok() {
        let e = build_checks(&base_probe());
        let e = find(&e, "emulator");
        assert_eq!(e.status, CheckStatus::Ok);
        assert!(e.detail.contains("1 AVD(s): glass"));
    }

    #[test]
    fn emulator_binary_absent_is_warn() {
        let mut p = base_probe();
        p.avds = None;
        let e = build_checks(&p);
        let e = find(&e, "emulator");
        assert_eq!(e.status, CheckStatus::Warn);
        assert!(e.detail.contains("emulator binary not found"));
        assert!(e.remedy.as_deref().unwrap().contains("ANDROID_SDK_ROOT"));
    }

    #[test]
    fn emulator_no_avds_is_warn() {
        let mut p = base_probe();
        p.avds = Some(vec![]);
        let e = build_checks(&p);
        let e = find(&e, "emulator");
        assert_eq!(e.status, CheckStatus::Warn);
        assert!(e.detail.contains("no AVDs"));
    }

    #[test]
    fn device_online_is_ok() {
        let d = build_checks(&base_probe());
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Ok);
        assert!(d.detail.contains("glass will use emulator-5554"));
    }

    #[test]
    fn device_attaches_to_selected_serial() {
        // With several online + GLASS_ANDROID_SERIAL set, the runtime attaches that one;
        // the doctor must report (and deep-probe) the selected serial, not online.first().
        // Drive `selection` through the real `decide` so the test tracks runtime behavior.
        let mut p = base_probe();
        p.online = vec!["emulator-5554".into(), "emulator-5556".into()];
        p.selection =
            decide(&[dev("emulator-5554"), dev("emulator-5556")], Some("emulator-5556"), Lifecycle::Auto);
        let d = build_checks(&p);
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Ok);
        assert!(d.detail.contains("glass will use emulator-5556"));
    }

    #[test]
    fn device_ambiguous_multi_without_serial_is_fail() {
        // Matches the runtime: multiple online + no GLASS_ANDROID_SERIAL => glass refuses,
        // so a green doctor never hides a start that will reject the host. Build the verdict
        // from the real `decide` so a change to its refusal message can't silently pass.
        let mut p = base_probe();
        p.online = vec!["a".into(), "b".into()];
        p.selection = decide(&[dev("a"), dev("b")], None, Lifecycle::Auto);
        let d = build_checks(&p);
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Fail);
        assert!(d.detail.contains("GLASS_ANDROID_SERIAL"));
    }

    #[test]
    fn device_attach_lifecycle_no_device_is_fail() {
        // Matches the runtime: lifecycle=attach + nothing online => glass won't boot.
        let mut p = base_probe();
        p.online = vec![];
        p.selection = decide(&[], None, Lifecycle::Attach);
        let d = build_checks(&p);
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Fail);
        assert!(d.detail.contains("GLASS_ANDROID_LIFECYCLE"));
    }

    #[test]
    fn device_none_online_but_bootable_is_warn() {
        let mut p = base_probe();
        p.online = vec![];
        p.selection = decide(&[], None, Lifecycle::Auto);
        let d = build_checks(&p);
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Warn);
        assert!(d.detail.contains("glass will boot"));
    }

    #[test]
    fn device_none_online_not_bootable_is_fail() {
        let mut p = base_probe();
        p.online = vec![];
        p.selection = decide(&[], None, Lifecycle::Auto); // would boot...
        p.avds = Some(vec![]); // ...but emulator ran with no AVDs => cannot boot
        let d = build_checks(&p);
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Fail);
        assert!(d.remedy.as_deref().unwrap().contains("emulator -avd"));
    }

    #[test]
    fn device_boot_but_no_emulator_binary_is_fail() {
        let mut p = base_probe();
        p.online = vec![];
        p.selection = decide(&[], None, Lifecycle::Auto); // would boot...
        p.avds = None; // ...but the emulator binary is absent => cannot boot
        let d = build_checks(&p);
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Fail);
        assert!(d.remedy.as_deref().unwrap().contains("emulator -avd"));
    }

    #[test]
    fn device_skipped_when_adb_absent() {
        let mut p = base_probe();
        p.adb_version = None;
        p.online = vec![];
        let d = build_checks(&p);
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Skip);
        assert!(d.detail.contains("adb unavailable"));
    }

    #[test]
    fn deep_not_requested_skips_capture_and_a11y() {
        let c = build_checks(&base_probe()); // deep_requested = false
        assert_eq!(find(&c, "screencap").status, CheckStatus::Skip);
        assert!(find(&c, "screencap").detail.contains("--deep"));
        assert_eq!(find(&c, "uiautomator").status, CheckStatus::Skip);
    }

    #[test]
    fn deep_requested_no_device_skips() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.online = vec![];
        p.selection = Action::Boot;
        p.deep = None;
        let c = build_checks(&p);
        assert_eq!(find(&c, "screencap").status, CheckStatus::Skip);
        assert!(find(&c, "screencap").detail.contains("no device selected"));
    }

    #[test]
    fn deep_adb_absent_skips_with_adb_reason() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.adb_version = None;
        p.deep = None;
        let c = build_checks(&p);
        assert!(find(&c, "screencap").detail.contains("adb unavailable"));
    }

    #[test]
    fn deep_ok_reports_ok() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.deep = Some(deep_ok());
        let c = build_checks(&p);
        assert_eq!(find(&c, "screencap").status, CheckStatus::Ok);
        assert!(find(&c, "screencap").detail.contains("emulator-5554"));
        assert!(find(&c, "screencap").detail.contains("1080x2400"));
        assert_eq!(find(&c, "uiautomator").status, CheckStatus::Ok);
    }

    #[test]
    fn deep_failure_reports_fail_with_remedy() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.deep = Some(DeepProbe {
            serial: "emulator-5554".into(),
            screencap: Err("screencap 0x0: FLAG_SECURE?".into()),
            uiautomator: Err("dump produced no hierarchy".into()),
        });
        let c = build_checks(&p);
        let s = find(&c, "screencap");
        assert_eq!(s.status, CheckStatus::Fail);
        assert!(s.detail.contains("FLAG_SECURE"));
        assert!(s.remedy.as_deref().unwrap().contains("fully booted"));
        assert_eq!(find(&c, "uiautomator").status, CheckStatus::Fail);
    }

    #[test]
    fn checks_always_emits_the_seven_named_checks() {
        // Spawns adb/emulator; both fail-fast when absent, so this is host-independent.
        let c = checks(false);
        let names: Vec<&str> = c.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["adb", "emulator", "device", "agent", "a11y-service", "screencap", "uiautomator"]);
    }

    #[test]
    fn agent_configured_passive_is_ok() {
        let c = build_checks(&base_probe());
        let a = find(&c, "agent");
        assert_eq!(a.status, CheckStatus::Ok);
        assert!(a.detail.contains("configured"));
    }

    #[test]
    fn agent_off_is_skip() {
        let mut p = base_probe();
        p.agent_off = true;
        assert_eq!(find(&build_checks(&p), "agent").status, CheckStatus::Skip);
    }

    #[test]
    fn agent_unset_is_skip() {
        let mut p = base_probe();
        p.agent_jar = None;
        p.agent_jar_exists = false;
        let a = build_checks(&p);
        let a = find(&a, "agent");
        assert_eq!(a.status, CheckStatus::Skip);
        assert!(a.detail.contains("GLASS_ANDROID_AGENT_JAR"));
    }

    #[test]
    fn agent_jar_missing_is_warn() {
        let mut p = base_probe();
        p.agent_jar = Some("/nope/glass-agent.jar".into());
        p.agent_jar_exists = false;
        let a = build_checks(&p);
        let a = find(&a, "agent");
        assert_eq!(a.status, CheckStatus::Warn);
        assert!(a.remedy.as_deref().unwrap().contains("gradlew"));
    }

    #[test]
    fn agent_deep_ok_is_ok() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.agent_deep = Some(Ok(()));
        let a = build_checks(&p);
        let a = find(&a, "agent");
        assert_eq!(a.status, CheckStatus::Ok);
        assert!(a.detail.contains("reachable"));
    }

    #[test]
    fn agent_deep_fail_is_fail() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.agent_deep = Some(Err("connect refused".into()));
        let a = build_checks(&p);
        let a = find(&a, "agent");
        assert_eq!(a.status, CheckStatus::Fail);
        assert!(a.detail.contains("connect refused"));
    }

    #[test]
    fn agent_deep_no_device_is_skip() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.selection = Action::Boot; // deep but nothing attachable → agent not probed
        p.agent_deep = None;
        assert_eq!(find(&build_checks(&p), "agent").status, CheckStatus::Skip);
    }

    #[test]
    fn a11y_configured_passive_is_ok() {
        let c = build_checks(&base_probe());
        let a = find(&c, "a11y-service");
        assert_eq!(a.status, CheckStatus::Ok);
        assert!(a.detail.contains("configured"));
    }

    #[test]
    fn a11y_off_is_skip() {
        let mut p = base_probe();
        p.a11y_off = true;
        assert_eq!(find(&build_checks(&p), "a11y-service").status, CheckStatus::Skip);
    }

    #[test]
    fn a11y_unset_is_skip() {
        let mut p = base_probe();
        p.a11y_apk = None;
        p.a11y_apk_exists = false;
        let a = build_checks(&p);
        let a = find(&a, "a11y-service");
        assert_eq!(a.status, CheckStatus::Skip);
        assert!(a.detail.contains("GLASS_ANDROID_A11Y_APK"));
    }

    #[test]
    fn a11y_apk_missing_is_warn() {
        let mut p = base_probe();
        p.a11y_apk = Some("/nope.apk".into());
        p.a11y_apk_exists = false;
        let a = build_checks(&p);
        let a = find(&a, "a11y-service");
        assert_eq!(a.status, CheckStatus::Warn);
        assert!(a.remedy.as_deref().unwrap().contains("gradlew"));
    }

    #[test]
    fn a11y_deep_ok_is_ok() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.a11y_deep = Some(Ok(()));
        let a = build_checks(&p);
        let a = find(&a, "a11y-service");
        assert_eq!(a.status, CheckStatus::Ok);
        assert!(a.detail.contains("reachable"));
    }

    #[test]
    fn a11y_deep_fail_is_fail() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.a11y_deep = Some(Err("connect refused".into()));
        let a = build_checks(&p);
        let a = find(&a, "a11y-service");
        assert_eq!(a.status, CheckStatus::Fail);
        assert!(a.detail.contains("connect refused"));
    }

    #[test]
    fn a11y_deep_no_device_is_skip() {
        let mut p = base_probe();
        p.deep_requested = true;
        p.selection = Action::Boot; // deep but nothing attachable → service not probed
        p.a11y_deep = None;
        assert_eq!(find(&build_checks(&p), "a11y-service").status, CheckStatus::Skip);
    }
}
