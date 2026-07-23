//! GTK4/GPU-toolkit apps render (not black) under containment on the headless X11 display —
//! proof that glass injects software-render env defaults for sandboxed launches. `#[ignore]d`
//! (needs Xvfb + python3-gi/GTK 4); run via `./scripts/test-x11.sh`.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::Xvfb;
use glass_core::{AppSpec, Platform, SandboxLevel, Stream};
use glass_x11::X11Platform;

const TASKS_DEMO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/tasks_demo.py");

/// Fail loudly if this host can't run the GTK4 demo (python3 + PyGObject + the GTK 4 typelib).
/// A hard requirement, not a silent skip: CI installs the packages, and a green pass here must
/// mean the rendering was actually verified. The dep-free wiring is covered by the backend unit
/// tests in `glass-x11`/`glass-wayland`.
fn require_gtk4() {
    let ok = Command::new("python3")
        .args([
            "-c",
            "import gi; gi.require_version('Gtk','4.0'); from gi.repository import Gtk",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        ok,
        "python3-gi / GTK 4 not available — install: sudo apt-get install python3-gi gir1.2-gtk-4.0"
    );
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

enum RenderOutcome {
    /// Painted a non-uniform frame.
    Rendered,
    /// Captured throughout, but every frame stayed uniform (e.g. all black).
    Blank,
    /// Capture never succeeded — the window/process likely died mid-poll.
    NoCapture,
}

/// Poll the window until it paints something (non-uniform) or the deadline passes. Distinguishes a
/// genuinely-blank window from one that stopped being capturable (a crash) — a swallowed capture
/// error must not read as "stayed blank".
fn poll_render(p: &mut X11Platform, deadline: Duration) -> RenderOutcome {
    let start = Instant::now();
    let mut captured_any = false;
    while start.elapsed() < deadline {
        match p.capture_frame(None) {
            Ok(frame) => {
                captured_any = true;
                if !is_uniform(&frame) {
                    return RenderOutcome::Rendered;
                }
            }
            Err(e) => eprintln!("capture_frame error while polling for render: {e}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if captured_any {
        RenderOutcome::Blank
    } else {
        RenderOutcome::NoCapture
    }
}

fn fmt_logs(logs: &[(Stream, String)]) -> String {
    if logs.is_empty() {
        return "(no app output captured)".into();
    }
    logs.iter()
        .map(|(s, line)| format!("[{s:?}] {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
#[ignore = "requires Xvfb + python3-gi/GTK 4; run via ./scripts/test-x11.sh"]
fn gtk4_app_renders_under_default_sandbox() {
    require_gtk4();
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&demo_spec(vec![]))
        .unwrap_or_else(|e| panic!("start_app failed: {e}"));
    // With the injected GSK_RENDERER=cairo default, GTK4 presents via plain X and renders.
    let outcome = poll_render(&mut p, Duration::from_secs(15));
    let logs = p.drain_logs();
    p.stop_app().ok();
    assert!(
        matches!(outcome, RenderOutcome::Rendered),
        "GTK4 app did not render under default containment — the software-render default was not \
         applied, or the app failed to start. App output:\n{}",
        fmt_logs(&logs)
    );
}

#[test]
#[ignore = "requires Xvfb + python3-gi/GTK 4; run via ./scripts/test-x11.sh"]
fn gtk4_gl_renderer_stays_black_under_sandbox() {
    require_gtk4();
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    // Force the GL renderer, overriding the injected cairo default. Under containment X11 MIT-SHM
    // can't attach (bwrap's --unshare-ipc), so the GL path can't present a frame — reproducing the
    // black window the fix targets, and proving an explicit spec.env entry overrides the default.
    p.start_app(&demo_spec(vec![("GSK_RENDERER".into(), "gl".into())]))
        .unwrap_or_else(|e| panic!("start_app failed: {e}"));
    let outcome = poll_render(&mut p, Duration::from_secs(6));
    let logs = p.drain_logs();
    p.stop_app().ok();
    match outcome {
        RenderOutcome::Blank => {} // Expected: window is alive but stays black.
        RenderOutcome::Rendered => panic!(
            "GTK4 GL renderer unexpectedly rendered under containment — the fixture no longer \
             reproduces the black-frame failure the fix guards against; revisit the fix's \
             necessity. App output:\n{}",
            fmt_logs(&logs)
        ),
        RenderOutcome::NoCapture => panic!(
            "capture never succeeded — the GL-forced app likely crashed instead of rendering \
             black, so this test can't confirm the black-frame behavior. App output:\n{}",
            fmt_logs(&logs)
        ),
    }
}
