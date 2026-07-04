//! Pure `.app`-bundle logic: detection, Info.plist resolution, and the fail-closed
//! containment gate. Host-agnostic (no FFI) so it unit-tests on any host, like
//! `glass-windows/src/discovery.rs`.
#![allow(dead_code)]

use glass_core::{platform::SandboxLevel, GlassError, Result};
use std::path::{Path, PathBuf};

/// True when `run[0]` names a `.app` bundle directory (the trigger for the NSWorkspace-capable
/// launch path). A plain executable path returns false and takes the unchanged direct-spawn path.
pub(crate) fn is_app_bundle(run0: &str) -> bool {
    let p = Path::new(run0);
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"))
        && p.join("Contents/Info.plist").is_file()
}

/// `Contents/MacOS/<CFBundleExecutable>` for the bundle.
pub(crate) fn resolve_inner_exec(bundle: &Path) -> Result<PathBuf> {
    let name = plist_string(bundle, "CFBundleExecutable")?;
    Ok(bundle.join("Contents/MacOS").join(name))
}

/// The bundle's `CFBundleIdentifier` (used to find the handed-off LaunchServices instance).
pub(crate) fn bundle_identifier(bundle: &Path) -> Result<String> {
    plist_string(bundle, "CFBundleIdentifier")
}

/// Fail-closed: a handed-off app can't be Seatbelt-contained, so only `Off` may adopt it.
pub(crate) fn handoff_gate(level: SandboxLevel) -> Result<()> {
    match level {
        SandboxLevel::Off => Ok(()),
        _ => Err(GlassError::AppNotStarted(
            "app handed off to LaunchServices and can't be Seatbelt-contained; \
             relaunch with sandbox:\"off\""
                .into(),
        )),
    }
}

fn plist_string(bundle: &Path, key: &str) -> Result<String> {
    let path = bundle.join("Contents/Info.plist");
    let val = plist::Value::from_file(&path)
        .map_err(|e| GlassError::AppNotStarted(format!("reading {}: {e}", path.display())))?;
    val.as_dictionary()
        .and_then(|d| d.get(key))
        .and_then(|v| v.as_string())
        .map(str::to_owned)
        .ok_or_else(|| GlassError::AppNotStarted(format!("{key} missing in {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_bundle(dir: &Path, info_plist: &str) -> PathBuf {
        let app = dir.join("Demo.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join("Contents/Info.plist"), info_plist).unwrap();
        app
    }

    const INFO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Demo</string>
<key>CFBundleIdentifier</key><string>tech.fixedwidth.demo</string>
</dict></plist>"#;

    #[test]
    fn detects_app_bundle_only_with_info_plist() {
        let tmp = tempfile::tempdir().unwrap();
        let app = write_bundle(tmp.path(), INFO);
        assert!(is_app_bundle(app.to_str().unwrap()));
        assert!(!is_app_bundle("/usr/bin/true"));
        assert!(!is_app_bundle(
            tmp.path().join("NoPlist.app").to_str().unwrap()
        ));
    }

    #[test]
    fn resolves_inner_exec_and_bundle_id() {
        let tmp = tempfile::tempdir().unwrap();
        let app = write_bundle(tmp.path(), INFO);
        assert_eq!(
            resolve_inner_exec(&app).unwrap(),
            app.join("Contents/MacOS/Demo")
        );
        assert_eq!(bundle_identifier(&app).unwrap(), "tech.fixedwidth.demo");
    }

    #[test]
    fn missing_key_is_structured_error() {
        let tmp = tempfile::tempdir().unwrap();
        let app = write_bundle(tmp.path(), "<plist version=\"1.0\"><dict></dict></plist>");
        assert!(matches!(
            resolve_inner_exec(&app),
            Err(GlassError::AppNotStarted(_))
        ));
    }

    #[test]
    fn handoff_gate_only_allows_off() {
        assert!(handoff_gate(SandboxLevel::Off).is_ok());
        assert!(matches!(
            handoff_gate(SandboxLevel::Default),
            Err(GlassError::AppNotStarted(_))
        ));
        assert!(matches!(
            handoff_gate(SandboxLevel::Strict),
            Err(GlassError::AppNotStarted(_))
        ));
    }
}
