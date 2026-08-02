#![cfg(target_os = "linux")]
//! MEASUREMENT, not an assertion: how often does X11's `start_app` return a geometry that
//! disagrees with a geometry re-read immediately afterwards? Whether X11 races the launch
//! geometry the way the macOS backend does (#263) decides whether the settle belongs here too
//! or stays macOS-local.
//!
//! Its own test target — not `tests/integration.rs`, which `scripts/test-x11.sh` runs on every
//! CI job — so this measurement's 20 extra cold launches per run don't ride along with output CI
//! discards. Run via `scripts/x11-geometry-settle-measurement.sh`.

mod common;

use common::Xvfb;
use glass_core::{AppSpec, Platform, WindowOp};
use glass_x11::X11Platform;

const TESTAPP: &str = env!("CARGO_BIN_EXE_glass-testapp");

fn app_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// macOS returns a mid-open-animation reading on most cold launches (#263). This measurement
/// uses `glass-testapp`, a fixed 320x240 window with no opening animation, under headless Xvfb —
/// an environment where such an animation cannot exist. A 0-of-20 result here means X11 doesn't
/// hand back a pre-map or stale geometry for a non-animating fixture; it isn't evidence the
/// backend can't race a real animating app. Asserts only that the launches succeeded; the rate is
/// for a human to read.
#[test]
#[ignore = "measurement; run via scripts/x11-geometry-settle-measurement.sh"]
fn geometry_settle_measurement() {
    const RUNS: usize = 20;
    let mut disagreed = 0usize;
    let xvfb = Xvfb::start();
    // Xvfb resets (and briefly refuses new connections) when its last client disconnects; kept
    // alive for the whole measurement so the per-run connect/drop below never takes the display
    // to zero clients and races that reset.
    let _keepalive = X11Platform::connect(Some(&xvfb.display)).expect("keepalive connect");
    for run in 1..=RUNS {
        // One `Xvfb` for the whole measurement, a fresh platform + launch per run — the fixture is
        // a plain executable, so every iteration is a genuine cold launch.
        let mut p = X11Platform::connect(Some(&xvfb.display)).expect("connect");
        let adopted = p.start_app(&app_spec()).expect("start_app");
        let reread = p.window(&WindowOp::Geometry).expect("window(Geometry)");
        if reread != adopted {
            disagreed += 1;
            println!("run {run}: adopted {adopted:?} != re-read {reread:?}");
        }
        p.stop_app().expect("stop_app");
    }
    println!("x11 geometry_settle_measurement: {disagreed} of {RUNS} launches disagreed");
}
