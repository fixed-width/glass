//! **On-box validation (Windows-only).** Drives the REAL `WindowsPlatform` pipeline
//! (`start_app` -> `capture_frame` -> `send_key` -> `stop_app`) for each sandbox level, so it
//! exercises the actual `resolve_containment` -> Sandboxie provider path (not a standalone
//! probe). Requires Sandboxie Classic installed for default/strict. `off` launches directly;
//! `default` launches in a Sandboxie box (network on); `strict` launches in a Sandboxie box
//! (no-egress) -- each should render + capture non-blank + take input. If Sandboxie is
//! unavailable, default/strict report `SandboxUnavailable` (fail-closed). No-op off Windows.
//! Run: cargo run -p glass-windows --example onbox_windows

// On-box FFI harness: opts out of the workspace `unsafe_code = "deny"` (each `unsafe` site is
// `// SAFETY:`-documented).
#![allow(unsafe_code)]

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox_windows` is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use glass_core::platform::{AppSpec, KeyEvent, Platform, WindowHint};
    use glass_core::SandboxLevel;
    use glass_windows::WindowsPlatform;

    fn is_blank(px: &[u8]) -> bool {
        match px.chunks_exact(4).next() {
            Some(first) => px.chunks_exact(4).all(|c| c == first),
            None => true,
        }
    }
    fn changed(a: &[u8], b: &[u8]) -> usize {
        a.chunks_exact(4)
            .zip(b.chunks_exact(4))
            .filter(|(x, y)| x != y)
            .count()
    }

    fn spec(level: SandboxLevel) -> AppSpec {
        AppSpec {
            build: None,
            run: vec!["charmap.exe".to_string()],
            cwd: None,
            env: vec![],
            window_hint: Some(WindowHint {
                title: Some("Character Map".into()),
                class: None,
            }),
            timeout_ms: 15_000,
            sandbox: level,
            a11y: false,
        }
    }

    fn run_level(level: SandboxLevel, label: &str) {
        println!("\n========== {label} ==========");
        let mut p = match WindowsPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                println!("  FAIL WindowsPlatform::new: {e}");
                return;
            }
        };
        let geo = match p.start_app(&spec(level)) {
            Ok(g) => {
                println!("  PASS start_app — geometry {g:?}");
                g
            }
            Err(e) => {
                // For strict/default when Sandboxie is absent this is the fail-closed path.
                println!("  start_app -> {e}");
                return;
            }
        };
        let _ = geo;
        std::thread::sleep(Duration::from_millis(1500));
        let f1 = match p.capture_frame(None) {
            Ok(f) => {
                let blank = is_blank(&f.pixels);
                println!(
                    "  capture {}  {}x{} blank={}",
                    if blank { "FAIL" } else { "PASS" },
                    f.width,
                    f.height,
                    blank
                );
                Some(f)
            }
            Err(e) => {
                println!("  capture FAIL {e}");
                None
            }
        };
        match p.send_key(&KeyEvent::Text("glass-onbox".into())) {
            Ok(()) => println!("  send_key PASS"),
            Err(e) => println!("  send_key FAIL {e}"),
        }
        std::thread::sleep(Duration::from_millis(900));
        if let (Some(f1), Ok(f2)) = (f1, p.capture_frame(None)) {
            if f1.pixels.len() == f2.pixels.len() {
                let n = changed(&f1.pixels, &f2.pixels);
                println!(
                    "  input-effect {}  changed_pixels={}",
                    if n > 0 { "PASS" } else { "FAIL (no change)" },
                    n
                );
            }
        }
        let _ = p.stop_app();
        println!("  stop_app done");
    }

    pub fn run() {
        println!("== glass-windows real-pipeline validation ==");
        // No embedded manifest in this example; make it Per-Monitor-V2 aware before capture/coords.
        // SAFETY: process-global DPI setting, no preconditions; ignored if already set.
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        run_level(SandboxLevel::Off, "off (unconfined)");
        run_level(SandboxLevel::Default, "default (Sandboxie, network on)");
        run_level(SandboxLevel::Strict, "strict (Sandboxie, no-egress)");
        println!("\n== done ==");
    }
}
