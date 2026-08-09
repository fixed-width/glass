//! glass#391: the a11y report resolves `at-spi-bus-launcher` through the crate that spawns it, so
//! it honours `GLASS_ATSPI_LAUNCHER` exactly as a launch does. Nothing else in the workspace can
//! see that delegation — hardcode `false` into `accessibility_launcher_present` and every other
//! test still passes.
//!
//! Its own test binary, holding this one test: it mutates the process environment, and the a11y
//! integration suite's other tests run glass sessions whose threads read the environment while
//! they live.

#![cfg(target_os = "linux")]

use std::ffi::OsString;

/// Restores the variable's prior value, not "unset" — a contributor whose launcher lives off the
/// well-known paths runs this suite with `GLASS_ATSPI_LAUNCHER` already set.
struct EnvGuard(Option<OsString>);

#[allow(unsafe_code)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: this binary holds exactly one test, so nothing else in the process is reading
        // the environment while it is mutated.
        match self.0.take() {
            Some(v) => unsafe { std::env::set_var("GLASS_ATSPI_LAUNCHER", v) },
            None => unsafe { std::env::remove_var("GLASS_ATSPI_LAUNCHER") },
        }
    }
}

#[test]
#[ignore = "mutates the process environment; run via scripts/test-a11y.sh"]
#[allow(unsafe_code)]
fn the_a11y_report_honours_the_launcher_override() {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = EnvGuard(std::env::var_os("GLASS_ATSPI_LAUNCHER"));

    let dir = tempfile::tempdir().expect("tempdir");
    let launcher = dir.path().join("at-spi-bus-launcher");
    std::fs::write(&launcher, b"").expect("write");
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // SAFETY: as above — one test in this binary.
    unsafe { std::env::set_var("GLASS_ATSPI_LAUNCHER", &launcher) };
    assert!(
        glass_a11y_linux::doctor::accessibility_launcher_present(),
        "a runnable launcher named by GLASS_ATSPI_LAUNCHER must count as present"
    );

    // SAFETY: as above.
    unsafe { std::env::set_var("GLASS_ATSPI_LAUNCHER", dir.path().join("nothing-here")) };
    assert!(
        !glass_a11y_linux::doctor::accessibility_launcher_present(),
        "an override that names nothing must not fall back to a system launcher"
    );
}
