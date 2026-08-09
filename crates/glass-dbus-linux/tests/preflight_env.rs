//! `available()` reads the environment, so the `available_with` seam tests cannot reach it — the
//! seam can be right while the wrapper hands it something else.
//!
//! Its own test binary: it mutates the process environment, and the crate's other tests spawn
//! buses whose threads read it while they live.

#![cfg(target_os = "linux")]

use std::ffi::OsString;

/// Restores the variable's prior value rather than unsetting it — both of these are documented
/// escape hatches an operator may already have set when running the suite.
struct EnvGuard(&'static str, Option<OsString>);

#[allow(unsafe_code)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: this binary holds exactly one test, so nothing else in the process is reading
        // the environment while it is mutated.
        match self.1.take() {
            Some(v) => unsafe { std::env::set_var(self.0, v) },
            None => unsafe { std::env::remove_var(self.0) },
        }
    }
}

#[allow(unsafe_code)]
fn set(name: &'static str, value: &str) -> EnvGuard {
    let guard = EnvGuard(name, std::env::var_os(name));
    // SAFETY: as above — one test in this binary.
    unsafe { std::env::set_var(name, value) };
    guard
}

#[test]
fn the_preflight_reports_a_dbus_daemon_that_is_not_there() {
    // A runnable launcher pins the other half of the preflight, so this passes on a host with no
    // at-spi2-core installed — which the workspace test job is.
    let _launcher = set("GLASS_ATSPI_LAUNCHER", "/bin/true");
    let _daemon = set("GLASS_DBUS_DAEMON", "/nonexistent/dbus-daemon");
    let err = glass_dbus_linux::available().expect_err("a missing dbus-daemon must be reported");
    assert!(err.contains("dbus-daemon"), "{err}");
}
