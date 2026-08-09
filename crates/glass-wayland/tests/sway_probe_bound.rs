//! glass#392, doctor's half: `glass doctor` asked a resolved sway for its version and waited
//! forever if it never answered.
//!
//! Nothing in the unit tests reaches this: they pass their own short budget, so
//! `VERSION_PROBE_BUDGET` could be raised to an hour, or doctor's probe returned to an unbounded
//! `Command::output()`, with every one of them still green.
//!
//! Its own test binary: it mutates the process environment, and the crate's other tests run
//! sessions whose threads read it while they live.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::time::{Duration, Instant};

/// How long the fixture lives if nothing kills it, in whole seconds. Far past the probe's budget,
/// so a lost bound fails this test rather than wedging the suite.
const FIXTURE_SECS: u64 = 30;

/// Restores the variable's prior value rather than unsetting it — an operator whose sway lives off
/// the well-known paths runs this suite with `GLASS_SWAY` already set.
struct EnvGuard(Option<OsString>);

#[allow(unsafe_code)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: this binary holds exactly one test, so nothing else in the process is reading
        // the environment while it is mutated.
        match self.0.take() {
            Some(v) => unsafe { std::env::set_var("GLASS_SWAY", v) },
            None => unsafe { std::env::remove_var("GLASS_SWAY") },
        }
    }
}

#[allow(unsafe_code)]
fn point_glass_sway_at(path: &std::path::Path) -> EnvGuard {
    let guard = EnvGuard(std::env::var_os("GLASS_SWAY"));
    // SAFETY: as above — one test in this binary.
    unsafe { std::env::set_var("GLASS_SWAY", path) };
    guard
}

/// A `sway` that accepts `--version` and then never answers it.
///
/// `exec`, so the sleeping process is the one glass spawned — a shell that forked its sleep would
/// leave the sleeper behind a kill that reached only the shell.
fn hung_sway(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("sway");
    std::fs::write(
        &bin,
        format!("#!/bin/sh\n[ \"$1\" = --version ] || exit 3\nexec sleep {FIXTURE_SECS}\n"),
    )
    .expect("write");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    bin
}

#[test]
fn doctor_reports_a_sway_that_never_answers_instead_of_waiting_for_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    // An override is the reachable case: `resolve_sway_verdict` version-probes only its `PATH`
    // walk, so a `GLASS_SWAY` binary reaches doctor never having been asked.
    let _guard = point_glass_sway_at(&hung_sway(dir.path()));

    let started = Instant::now();
    let checks = glass_wayland::doctor::checks(false);
    assert!(
        started.elapsed() < Duration::from_secs(FIXTURE_SECS) / 2,
        "doctor waited out the binary rather than bounding it: {:?}",
        started.elapsed()
    );

    let sway = checks
        .iter()
        .find(|c| c.name == "sway >=1.12")
        .expect("doctor reports on sway");
    assert_ne!(
        sway.status,
        glass_core::CheckStatus::Ok,
        "a sway that never answered is not a proven >=1.12: {sway:?}"
    );
    assert!(sway.detail.contains("did not answer"), "{sway:?}");
    assert!(sway.remedy.is_some(), "{sway:?}");
}
