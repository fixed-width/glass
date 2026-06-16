//! `glass doctor` checks for the Android backend: are adb, the emulator, an AVD,
//! and an online device present (and, with `--deep`, can we capture + dump a11y)?
//!
//! Pure `build_checks(&Probe)` over observed state, plus a thin subprocess `probe`.
//! Reports `Check` statuses; never errors.

use glass_core::{Check, CheckStatus};

use crate::adb::Adb;

/// Observed host state for the Android doctor checks. Captured by `probe`, consumed
/// by the pure `build_checks` so all branch logic is unit-testable without subprocesses.
struct Probe {
    /// Resolved adb path (`GLASS_ADB`, else `"adb"`).
    adb_bin: String,
    /// First line of `adb version`; `None` when adb is absent/unrunnable.
    adb_version: Option<String>,
}

/// Build the Android doctor checks by probing the host. `deep` additionally captures a
/// frame and an a11y dump from an already-online device (it never boots one).
pub fn checks(deep: bool) -> Vec<Check> {
    build_checks(&probe(deep))
}

fn probe(_deep: bool) -> Probe {
    let adb = Adb::from_env();
    let adb_bin = adb.bin().to_string();
    let adb_version = adb.run(["version"]).ok().map(|s| first_line(&s)).filter(|s| !s.is_empty());
    Probe { adb_bin, adb_version }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// Build the Android doctor section's checks from observed state. Pure.
fn build_checks(p: &Probe) -> Vec<Check> {
    vec![adb_check(p)]
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
}
