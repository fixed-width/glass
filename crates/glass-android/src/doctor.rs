//! `glass doctor` checks for the Android backend: are adb, the emulator, an AVD,
//! and an online device present (and, with `--deep`, can we capture + dump a11y)?
//!
//! Pure `build_checks(&Probe)` over observed state, plus a thin subprocess `probe`.
//! Reports `Check` statuses; never errors.

use glass_core::{Check, CheckStatus};

use crate::adb::Adb;
use crate::avd::{parse_list_avds, resolve_emulator_bin};
use crate::axmap::check_dump_status;
use crate::target::parse_devices;

/// Observed host state for the Android doctor checks. Captured by `probe`, consumed
/// by the pure `build_checks` so all branch logic is unit-testable without subprocesses.
struct Probe {
    /// Resolved adb path (`GLASS_ADB`, else `"adb"`).
    adb_bin: String,
    /// First line of `adb version`; `None` when adb is absent/unrunnable.
    adb_version: Option<String>,
    /// Resolved emulator path (`GLASS_EMULATOR`/SDK root/`"emulator"`).
    emulator_bin: String,
    /// AVDs from `emulator -list-avds`; `None` when the binary is absent/failed,
    /// `Some(vec![])` when it ran but found none.
    avds: Option<Vec<String>>,
    /// Serials with `adb devices` state `"device"` (online).
    online: Vec<String>,
    /// Whether `--deep` was requested (so `build_checks` can pick the right Skip reason).
    deep_requested: bool,
    /// Deep-probe results; `Some` only when `deep_requested` && adb present && a device is online.
    deep: Option<DeepProbe>,
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
    let adb = Adb::from_env();
    let adb_bin = adb.bin().to_string();
    let adb_version = adb.run(["version"]).ok().map(|s| first_line(&s)).filter(|s| !s.is_empty());
    let emulator_bin = resolve_emulator_bin(&get);
    let avds = list_avds(&emulator_bin);
    let online: Vec<String> = if adb_version.is_some() {
        parse_devices(&adb.run(["devices"]).unwrap_or_default())
            .into_iter()
            .filter(|d| d.state == "device")
            .map(|d| d.serial)
            .collect()
    } else {
        Vec::new()
    };
    let deep = if deep_requested && adb_version.is_some() {
        online.first().map(|serial| deep_probe(&adb, serial))
    } else {
        None
    };
    Probe { adb_bin, adb_version, emulator_bin, avds, online, deep_requested, deep }
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
            Ok(f) => Ok(format!("captured {}x{} ({} bytes)", f.width, f.height, bytes.len())),
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
    vec![adb_check(p), emulator_check(p), device_check(p), screencap, uiautomator]
}

fn deep_checks(p: &Probe) -> (Check, Check) {
    let skip = |reason: &str| {
        (
            Check::new("screencap", CheckStatus::Skip, reason.to_string()),
            Check::new("uiautomator", CheckStatus::Skip, reason.to_string()),
        )
    };
    // Distinguish the three "nothing to probe" cases (see spec): adb first, then the
    // missing flag, then the absent device (post-guards, `deep.is_none()` ⟺ no online device).
    if p.adb_version.is_none() {
        return skip("skipped — adb unavailable");
    }
    if !p.deep_requested {
        return skip("run with --deep to probe capture");
    }
    let Some(d) = &p.deep else {
        return skip("no online device to probe");
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
    if !p.online.is_empty() {
        return Check::new(
            "device",
            CheckStatus::Ok,
            format!("{} online: {}", p.online.len(), p.online.join(", ")),
        );
    }
    let bootable = matches!(&p.avds, Some(avds) if !avds.is_empty());
    if bootable {
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
        .with_remedy("install the Android emulator package, or set GLASS_EMULATOR / ANDROID_SDK_ROOT"),
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
        Some(v) => Check::new("adb", CheckStatus::Ok, format!("{} ({v})", p.adb_bin)),
        None => Check::new(
            "adb",
            CheckStatus::Fail,
            format!("`adb` not found or not runnable ({})", p.adb_bin),
        )
        .with_remedy("install Android platform-tools, or set GLASS_ADB to the adb binary"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_probe() -> Probe {
        Probe {
            adb_bin: "/sdk/platform-tools/adb".into(),
            adb_version: Some("Android Debug Bridge version 1.0.41".into()),
            emulator_bin: "/sdk/emulator/emulator".into(),
            avds: Some(vec!["glass".into()]),
            online: vec!["emulator-5554".into()],
            deep_requested: false,
            deep: None,
        }
    }

    fn deep_ok() -> DeepProbe {
        DeepProbe {
            serial: "emulator-5554".into(),
            screencap: Ok("captured 1080x2400 (10368016 bytes)".into()),
            uiautomator: Ok("a11y dump OK".into()),
        }
    }

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).expect("check present")
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
        assert!(d.detail.contains("1 online: emulator-5554"));
    }

    #[test]
    fn device_none_online_but_bootable_is_warn() {
        let mut p = base_probe();
        p.online = vec![];
        let d = build_checks(&p);
        let d = find(&d, "device");
        assert_eq!(d.status, CheckStatus::Warn);
        assert!(d.detail.contains("glass will boot"));
    }

    #[test]
    fn device_none_online_not_bootable_is_fail() {
        let mut p = base_probe();
        p.online = vec![];
        p.avds = Some(vec![]); // emulator ran, no AVDs => cannot boot
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
        p.deep = None;
        let c = build_checks(&p);
        assert_eq!(find(&c, "screencap").status, CheckStatus::Skip);
        assert!(find(&c, "screencap").detail.contains("no online device"));
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
    fn checks_always_emits_the_five_named_checks() {
        // Spawns adb/emulator; both fail-fast when absent, so this is host-independent.
        let c = checks(false);
        let names: Vec<&str> = c.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["adb", "emulator", "device", "screencap", "uiautomator"]);
    }
}
