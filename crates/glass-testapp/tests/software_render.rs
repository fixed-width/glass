//! GTK4/GPU-toolkit apps render (not black) under containment on the headless X11 display —
//! proof that glass injects software-render env defaults for sandboxed launches. `#[ignore]d`
//! (needs Xvfb + python3-gi/GTK 4); run via `./scripts/test-x11.sh`.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::Xvfb;
use glass_core::{AppSpec, Platform, SandboxLevel};
use glass_x11::X11Platform;

const TASKS_DEMO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/tasks_demo.py");

/// True if this host can run the GTK4 demo (python3 + PyGObject + the GTK 4 typelib).
fn gtk4_available() -> bool {
    Command::new("python3")
        .args([
            "-c",
            "import gi; gi.require_version('Gtk','4.0'); from gi.repository import Gtk",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn demo_spec(env: Vec<(String, String)>) -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["python3".into(), TASKS_DEMO.into()],
        cwd: None,
        env,
        window_hint: None,
        timeout_ms: 15_000,
        sandbox: SandboxLevel::Default,
        a11y: false,
    }
}

/// All pixels identical → a uniform (e.g. all-black) frame.
fn is_uniform(frame: &glass_core::Frame) -> bool {
    let Some(first) = frame.pixels.get(0..4) else {
        return true;
    };
    frame.pixels.chunks_exact(4).all(|px| px == first)
}

/// Poll the window until it paints something (a non-uniform frame) or the deadline passes.
fn rendered_within(p: &mut X11Platform, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if let Ok(frame) = p.capture_frame(None) {
            if !is_uniform(&frame) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[test]
#[ignore = "requires Xvfb + python3-gi/GTK 4; run via ./scripts/test-x11.sh"]
fn gtk4_app_renders_under_default_sandbox() {
    if !gtk4_available() {
        eprintln!("skipping: python3-gi / GTK 4 not available (install python3-gi gir1.2-gtk-4.0)");
        return;
    }
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&demo_spec(vec![]))
        .unwrap_or_else(|e| panic!("start_app failed: {e}"));
    // With the injected GSK_RENDERER=cairo default, GTK4 renders on the headless display.
    let rendered = rendered_within(&mut p, Duration::from_secs(8));
    p.stop_app().ok();
    assert!(
        rendered,
        "GTK4 app stayed blank under default containment — software-render default not applied"
    );
}

#[test]
#[ignore = "requires Xvfb + python3-gi/GTK 4; run via ./scripts/test-x11.sh"]
fn gtk4_gl_renderer_stays_black_under_sandbox() {
    if !gtk4_available() {
        eprintln!("skipping: python3-gi / GTK 4 not available (install python3-gi gir1.2-gtk-4.0)");
        return;
    }
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    // Force the GL renderer, overriding the injected cairo default. This both reproduces the
    // failure the fix exists for (GL can't attach MIT-SHM or reach a GPU under containment) and
    // proves an explicit spec.env entry overrides the default.
    p.start_app(&demo_spec(vec![("GSK_RENDERER".into(), "gl".into())]))
        .unwrap_or_else(|e| panic!("start_app failed: {e}"));
    let rendered = rendered_within(&mut p, Duration::from_secs(6));
    p.stop_app().ok();
    assert!(
        !rendered,
        "GTK4 GL renderer unexpectedly rendered under containment — the fixture no longer \
         reproduces the black-frame failure the fix guards against; revisit the fix's necessity"
    );
}
