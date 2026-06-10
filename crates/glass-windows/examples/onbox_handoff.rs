//! On-box validation for the broker/handoff grace period in `discover_window` (Windows-only).
//!
//! Win11's packaged Notepad launches `notepad.exe`, which hands its UI to a broker-spawned
//! process and exits — so the real window is owned by neither the launcher nor a Job/Toolhelp
//! descendant. This exercises both branches of `discovery::poll_decision`:
//!   [A] WITH a title hint  -> should ADOPT the handoff window (the fix), polling past root-exit.
//!   [B] WITHOUT a hint      -> should FAST-FAIL `AppExited` (crash detection, unchanged).
//!
//! Must run in the interactive desktop session (session 1) — over SSH (session 0) drive it via
//! the scheduled-task bridge:
//!   cargo run -p glass-windows --example onbox_handoff

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox_handoff` example is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use glass_core::{AppSpec, GlassError, Platform, WindowHint};
    use glass_windows::WindowsPlatform;
    use std::time::{Duration, Instant};

    fn kill_notepad() {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "notepad.exe", "/T"])
            .output();
        std::thread::sleep(Duration::from_millis(800));
    }

    fn spec(hint: Option<WindowHint>, timeout_ms: u64) -> AppSpec {
        AppSpec {
            build: None,
            run: vec!["notepad.exe".to_string()],
            cwd: None,
            env: vec![],
            window_hint: hint,
            timeout_ms,
            sandbox: glass_core::SandboxLevel::Off,
        }
    }

    fn is_blank(px: &[u8]) -> bool {
        match px.chunks_exact(4).next() {
            Some(first) => px.chunks_exact(4).all(|c| c == first),
            None => true,
        }
    }

    pub fn run() {
        println!("== glass-windows handoff/grace on-box validation ==");
        // Standalone example (no embedded manifest): opt into Per-Monitor-V2 before any capture.
        // SAFETY: process-global, no preconditions; harmless if already set.
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }

        // ---- [A] WITH a title hint -> expect ADOPT (the fix) ----
        println!("\n[A] start_app notepad.exe WITH title hint \"Notepad\" (timeout 8s)");
        kill_notepad();
        let mut pa = WindowsPlatform::new().expect("new");
        let t = Instant::now();
        let res_a =
            pa.start_app(&spec(Some(WindowHint { title: Some("Notepad".into()), class: None }), 8_000));
        let el = t.elapsed().as_millis();
        match &res_a {
            Ok(g) => println!("  PASS  adopted handoff window in {el} ms  geometry = {g:?}"),
            Err(e) => println!("  FAIL  {e}  (after {el} ms)"),
        }
        if res_a.is_ok() {
            match pa.list_windows() {
                Ok(ws) => {
                    println!("  list_windows: {} window(s)", ws.len());
                    for w in &ws {
                        println!(
                            "    active={} title={:?} class={:?} geo={:?}",
                            w.active, w.title, w.class, w.geometry
                        );
                    }
                }
                Err(e) => println!("  list_windows FAIL {e}"),
            }
            match pa.capture_frame(None) {
                Ok(f) => {
                    let blank = is_blank(&f.pixels);
                    println!(
                        "  capture: {}x{} blank={} ({})",
                        f.width,
                        f.height,
                        blank,
                        if blank { "FAIL blank" } else { "PASS non-blank" }
                    );
                }
                Err(e) => println!("  capture FAIL {e}"),
            }
        }
        let _ = pa.stop_app();
        kill_notepad();

        // ---- [B] WITHOUT a hint -> expect fast-fail AppExited (unchanged) ----
        println!("\n[B] start_app notepad.exe WITHOUT hint (timeout 8s) -> expect fast AppExited");
        let mut pb = WindowsPlatform::new().expect("new");
        let t = Instant::now();
        let res_b = pb.start_app(&spec(None, 8_000));
        let el = t.elapsed().as_millis();
        match &res_b {
            Ok(g) => println!(
                "  UNEXPECTED Ok in {el} ms  geometry = {g:?}  (a stray notepad window owned by our \
                 pid-set? — check)"
            ),
            Err(GlassError::AppExited(code)) => {
                let fast = el < 4_000;
                println!(
                    "  {}  AppExited({code:?}) in {el} ms  ({})",
                    if fast { "PASS" } else { "CHECK" },
                    if fast {
                        "fast-fail preserved"
                    } else {
                        "slower than expected — did it wait the timeout?"
                    }
                );
            }
            Err(e) => println!("  CHECK  unexpected error {e}  (after {el} ms)"),
        }
        let _ = pb.stop_app();
        kill_notepad();

        println!("\n== done ==");
    }
}
