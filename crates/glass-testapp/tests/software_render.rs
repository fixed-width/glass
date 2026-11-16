//! GTK4/GPU-toolkit apps render (not black) under containment on the headless X11 display —
//! proof that glass injects software-render env defaults for sandboxed launches. `#[ignore]d`
//! (needs Xvfb + python3-gi/GTK 4); run via `./scripts/test-x11.sh`.

#![cfg(target_os = "linux")]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::Xvfb;
use glass_core::{AppSpec, Platform, SandboxLevel, Stream};
use glass_x11::X11Platform;

const RENDERER_PROBE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/renderer_probe.py"
);

/// CI installs the GTK runtime, so missing prerequisites fail rather than skip rendering checks.
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

fn renderer_spec(env: Vec<(String, String)>) -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["python3".into(), RENDERER_PROBE.into()],
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
    frame.pixels.as_chunks::<4>().0.iter().all(|px| px == first)
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

fn renderer_logs(p: &mut X11Platform) -> Vec<(Stream, String)> {
    let started = Instant::now();
    let mut logs = Vec::new();
    while started.elapsed() < Duration::from_secs(6) {
        logs.extend(p.drain_logs());
        if logs.iter().any(|(_, line)| line.starts_with("RENDERER ")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    logs
}

fn reported_renderer(logs: &[(Stream, String)], requested: &str, suffix: &str) -> bool {
    let prefix = format!("RENDERER requested={requested} actual=");
    logs.iter()
        .any(|(_, line)| line.starts_with(&prefix) && line.to_ascii_lowercase().ends_with(suffix))
}

#[test]
#[ignore = "requires Xvfb + python3-gi/GTK 4; run via ./scripts/test-x11.sh"]
fn gtk4_app_renders_under_default_sandbox() {
    require_gtk4();
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&renderer_spec(vec![]))
        .unwrap_or_else(|e| panic!("start_app failed: {e}"));
    let mut logs = renderer_logs(&mut p);
    let outcome = poll_render(&mut p, Duration::from_secs(15));
    logs.extend(p.drain_logs());
    p.stop_app().expect("stop renderer probe");
    assert!(
        reported_renderer(&logs, "cairo", "cairorenderer"),
        "GTK did not select the default Cairo renderer:\n{}",
        fmt_logs(&logs)
    );
    assert!(
        matches!(outcome, RenderOutcome::Rendered),
        "GTK4 app did not render under default containment — the software-render default was not \
         applied, or the app failed to start. App output:\n{}",
        fmt_logs(&logs)
    );
}

#[test]
#[ignore = "requires Xvfb + python3-gi/GTK 4; run via ./scripts/test-x11.sh"]
fn gtk4_renderer_override_selects_gl_under_sandbox() {
    require_gtk4();
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&renderer_spec(vec![("GSK_RENDERER".into(), "gl".into())]))
        .unwrap_or_else(|e| panic!("start_app failed: {e}"));
    let logs = renderer_logs(&mut p);
    let frame = p.capture_frame(None);
    p.stop_app().expect("stop renderer probe");
    assert!(
        frame.is_ok(),
        "GL override produced no capturable window:\n{}",
        fmt_logs(&logs)
    );
    assert!(
        reported_renderer(&logs, "gl", "glrenderer"),
        "GTK did not select the explicitly requested GL renderer:\n{}",
        fmt_logs(&logs)
    );
}
