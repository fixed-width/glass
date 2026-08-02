//! MEASUREMENT, not an assertion: how often does Wayland's `start_app` return a geometry that
//! disagrees with a geometry re-read immediately afterwards? Whether Wayland races the launch
//! geometry the way the macOS backend does (#263) decides whether the settle belongs here too
//! or stays macOS-local.
//!
//! Its own test target — not `tests/wayland.rs`, which `scripts/test-wayland.sh` runs on every
//! CI job — so this measurement's 20 extra cold launches + sway spawns per run don't ride along
//! with output CI discards. Run via `scripts/wayland-geometry-settle-measurement.sh`.

#![cfg(target_os = "linux")]

use glass_core::{AppSpec, Platform, WindowOp};
use glass_wayland::WaylandPlatform;

const TESTAPP: &str = env!("CARGO_BIN_EXE_glass-testapp");
const APP_TIMEOUT_MS: u64 = 15_000; // start_app: wait this long for sway's socket

fn spec(run: Vec<String>, timeout_ms: u64) -> AppSpec {
    AppSpec {
        build: None,
        run,
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// `start_app`, but dump the captured sway/Xwayland/app logs before panicking on failure —
/// mirrors `tests/wayland.rs`'s identically-named helper.
fn start(p: &mut WaylandPlatform, spec: &AppSpec) -> glass_core::WindowGeometry {
    match p.start_app(spec) {
        Ok(geom) => geom,
        Err(e) => {
            eprintln!("\nstart_app failed: {e}\n--- captured sway/Xwayland/app logs ---");
            for (stream, line) in p.drain_logs() {
                eprintln!("  [{stream:?}] {line}");
            }
            eprintln!("--- end captured logs ---");
            panic!("start_app failed: {e}");
        }
    }
}

/// macOS returns a mid-open-animation reading on most cold launches (#263). This measurement
/// uses `glass-testapp`, a fixed 320x240 window with no opening animation, under headless sway —
/// an environment where such an animation cannot exist. A 0-of-20 result here means Wayland
/// doesn't hand back a pre-map or stale geometry for a non-animating fixture; it isn't evidence
/// the backend can't race a real animating app. Asserts only that the launches succeeded; the
/// rate is for a human to read.
#[test]
#[ignore = "measurement; run via scripts/wayland-geometry-settle-measurement.sh"]
fn geometry_settle_measurement() {
    const RUNS: usize = 20;
    let mut disagreed = 0usize;
    for run in 1..=RUNS {
        // A fresh sway compositor + platform + launch per run — the fixture is a plain
        // executable, so every iteration is a genuine cold launch.
        let mut p = WaylandPlatform::new().unwrap();
        let adopted = start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
        let reread = p.window(&WindowOp::Geometry).unwrap();
        if reread != adopted {
            disagreed += 1;
            println!("run {run}: adopted {adopted:?} != re-read {reread:?}");
        }
        p.stop_app().unwrap();
    }
    println!("wayland geometry_settle_measurement: {disagreed} of {RUNS} launches disagreed");
}
