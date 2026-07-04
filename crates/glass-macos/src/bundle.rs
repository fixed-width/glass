//! Pure `.app`-bundle logic: detection, Info.plist resolution, and the fail-closed
//! containment gate. Host-agnostic (no FFI) so it unit-tests on any host, like
//! `glass-windows/src/discovery.rs`.
#![forbid(unsafe_code)]
// These fns are consumed by the cfg(macos) launch path (tasks 2/3) and exercised by the
// unit tests below, so off-macOS non-test builds see them as dead. Keep the lint live on
// macOS. Same pattern as `glass-windows/src/doctor.rs`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use glass_core::{platform::SandboxLevel, GlassError, Result};
use std::path::{Component, Path, PathBuf};

/// True when `run[0]` names a `.app` bundle directory (the trigger for the NSWorkspace-capable
/// launch path). A plain executable path returns false and takes the unchanged direct-spawn path.
pub(crate) fn is_app_bundle(run0: &str) -> bool {
    let p = Path::new(run0);
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"))
        && p.join("Contents/Info.plist").is_file()
}

/// `Contents/MacOS/<CFBundleExecutable>` for the bundle. The executable name must be a
/// bare filename — the resolved path is `exec`'d by a later task, so a value that is
/// absolute or contains a separator / `..` traversal is rejected as a structured error.
pub(crate) fn resolve_inner_exec(bundle: &Path) -> Result<PathBuf> {
    let name = plist_string(bundle, "CFBundleExecutable")?;
    if !is_bare_filename(&name) {
        return Err(GlassError::AppNotStarted(format!(
            "CFBundleExecutable {name:?} is not a bare filename in {}",
            bundle.join("Contents/Info.plist").display()
        )));
    }
    Ok(bundle.join("Contents/MacOS").join(name))
}

/// True only when `name` is a single normal path component: not absolute, no separators,
/// no `.`/`..` traversal. Guards the `CFBundleExecutable` value before it is joined and
/// handed to `exec`.
fn is_bare_filename(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(c)), None) if c.as_encoded_bytes() == name.as_bytes()
    )
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

/// Whether an app adopted via the LaunchServices handoff path (`backend.rs`'s
/// `start_bundle`) is one glass itself started on this call — and so must reap when the
/// session ends — or one it merely re-found already running, which it must leave alive.
/// Carried in [`backend::MacosPlatform`]'s `Adopted` record and consulted by
/// `stop_app`/`Drop` via [`Disposition::should_terminate`].
///
/// Derivation caveat: this can't distinguish a genuinely pre-existing user instance from a
/// stub-spawned LaunchServices copy that briefly raced ahead of the adoption lookup. The
/// derivation is therefore biased deliberately toward the safer `PreExisting` ("leave the
/// app alive") whenever it can't be certain glass started the instance — never toward
/// terminating an app glass may not own.
#[derive(Clone, Copy)]
pub(crate) enum Disposition {
    /// This call's own `ffi::launch_bundle` started the app — glass owns its lifetime, so
    /// it is terminated when the session ends.
    Fresh,
    /// `ffi::running_pid_for_bundle_id` found an instance already running before this call —
    /// glass only raised it, so it is left running.
    PreExisting,
}

impl Disposition {
    /// Pure reap predicate: only a `Fresh` adoption is terminated on `stop_app`/`Drop`. Kept
    /// as a standalone predicate (rather than an inline `if fresh`) so the "which
    /// disposition gets terminated" decision has one unit-testable definition — a boolean
    /// inversion here would terminate a user's pre-existing app, so it carries a dedicated
    /// off-macOS test (see `disposition_only_terminates_fresh`).
    pub(crate) fn should_terminate(self) -> bool {
        matches!(self, Disposition::Fresh)
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
        // A plain executable path is not a bundle.
        assert!(!is_app_bundle("/usr/bin/true"));
        // A `.app` path that doesn't exist is not a bundle.
        assert!(!is_app_bundle(
            tmp.path().join("NoPlist.app").to_str().unwrap()
        ));
        // An existing `.app` directory MISSING `Contents/Info.plist` is not a bundle.
        let empty_app = tmp.path().join("Empty.app");
        fs::create_dir_all(empty_app.join("Contents/MacOS")).unwrap();
        assert!(!is_app_bundle(empty_app.to_str().unwrap()));
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
    fn non_bare_executable_name_is_rejected() {
        // Every non-bare `CFBundleExecutable` form must be rejected before the resolved path
        // is ever handed to `exec`. Each string exercises a different branch of
        // `is_bare_filename`:
        //   - `../../etc/evil` — a `..` traversal (leading `ParentDir` component)
        //   - `/etc/evil`      — a bare absolute path (leading `RootDir` component)
        //   - `sub/exec`       — a nested, non-traversing name (two `Normal` components, so
        //                        the second `components.next()` is `Some`, not `None`)
        //   - `Demo/`          — a trailing slash: one `Normal` component, but its
        //                        reconstructed bytes ("Demo") differ from the raw string
        //                        ("Demo/"), caught by the byte-equality guard
        let tmp = tempfile::tempdir().unwrap();
        for evil in ["../../etc/evil", "/etc/evil", "sub/exec", "Demo/"] {
            let app = write_bundle(
                tmp.path(),
                &format!(
                    r#"<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{evil}</string>
</dict></plist>"#
                ),
            );
            assert!(
                matches!(resolve_inner_exec(&app), Err(GlassError::AppNotStarted(_))),
                "expected {evil:?} to be rejected as a non-bare CFBundleExecutable"
            );
        }
    }

    #[test]
    fn unparseable_plist_is_structured_error() {
        // A genuinely malformed Info.plist (not merely one missing a key) must surface as a
        // structured `AppNotStarted` from the `plist::Value::from_file` read itself — a
        // distinct failure from `missing_key_is_structured_error` above, which parses fine
        // but lacks the key.
        let tmp = tempfile::tempdir().unwrap();
        let app = write_bundle(tmp.path(), "not a plist");
        assert!(matches!(
            resolve_inner_exec(&app),
            Err(GlassError::AppNotStarted(_))
        ));
    }

    #[test]
    fn disposition_only_terminates_fresh() {
        // Pure CI backstop for the reap decision (runs off macOS): a boolean inversion here
        // would make `stop_app`/`Drop` terminate a user's pre-existing app (`PreExisting`) or
        // orphan a glass-started one (`Fresh`).
        assert!(Disposition::Fresh.should_terminate());
        assert!(!Disposition::PreExisting.should_terminate());
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
