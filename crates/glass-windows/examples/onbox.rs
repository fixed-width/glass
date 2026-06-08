//! On-box validation harness for the glass-windows backend (Windows-only).
//!
//! Drives [`glass_windows::WindowsPlatform`] through the build → see → interact → debug loop on
//! Notepad and writes PNGs + a text report to `C:\Users\mpd`. It MUST run in an interactive
//! desktop session (session 1) — WGC capture and `SendInput` need the active input desktop, so
//! over SSH (session 0) it is driven via the scheduled-task bridge:
//!   cargo run -p glass-windows --example onbox
//!
//! On non-Windows hosts it is a no-op, so `cargo test` / `clippy --all-targets` stay green on the
//! Linux dev box.

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox` example is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use glass_core::{AppSpec, KeyEvent, MouseButton, Platform, PointerEvent, WindowHint, WindowOp};
    use glass_windows::WindowsPlatform;
    use std::time::Duration;

    const OUT: &str = "C:\\Users\\mpd";

    /// True if every pixel is identical (a uniform/blank frame — WGC didn't return real pixels).
    fn is_blank(px: &[u8]) -> bool {
        match px.chunks_exact(4).next() {
            Some(first) => px.chunks_exact(4).all(|c| c == first),
            None => true,
        }
    }
    /// Count of differing pixels between two equal-size RGBA buffers.
    fn changed(a: &[u8], b: &[u8]) -> usize {
        a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(x, y)| x != y).count()
    }
    fn save(name: &str, w: u32, h: u32, rgba: &[u8]) {
        let path = format!("{OUT}\\{name}");
        match image::save_buffer(&path, rgba, w, h, image::ColorType::Rgba8) {
            Ok(()) => println!("    saved {path}"),
            Err(e) => println!("    save {path} FAILED: {e}"),
        }
    }

    pub fn run() {
        println!("== glass-windows on-box validation ==");

        // This standalone example has no embedded manifest (that ships on glass-mcp.exe), so make the
        // process Per-Monitor-V2 aware at runtime BEFORE any capture/coords — otherwise a scaled monitor
        // virtualizes them. Must precede any DPI-sensitive call.
        // SAFETY: SetProcessDpiAwarenessContext is a process-global setting with no preconditions; it
        // only fails (harmlessly, ignored) if awareness was already set, which nothing here does first.
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }

        println!("\n[doctor --deep]");
        for c in glass_windows::doctor::checks(true) {
            let r = c.remedy.as_deref().map(|r| format!("   remedy: {r}")).unwrap_or_default();
            println!("  {:?}  {} — {}{}", c.status, c.name, c.detail, r);
        }

        let mut p = match WindowsPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                println!("FATAL WindowsPlatform::new: {e}");
                return;
            }
        };
        // charmap.exe (Character Map): a classic single-process Win32 app — unlike Win11's packaged
        // Notepad, it doesn't hand off to an out-of-process Store app and exit, and it ignores stdin so
        // it stays open. Ideal for exercising capture / input / window-ops / kill-tree.
        let spec = AppSpec {
            build: None,
            run: vec!["charmap.exe".to_string()],
            cwd: None,
            env: vec![],
            window_hint: Some(WindowHint { title: Some("Character Map".into()), class: None }),
            timeout_ms: 12_000,
            sandbox: glass_core::SandboxLevel::Off,
        };

        println!("\n[start_app charmap]");
        let started = match p.start_app(&spec) {
            Ok(g) => {
                println!("  PASS  geometry = {g:?}");
                true
            }
            Err(e) => {
                println!("  FAIL  {e}");
                false
            }
        };
        if !started {
            println!("\n== aborted: no window ==");
            return;
        }
        std::thread::sleep(Duration::from_millis(1500));

        println!("\n[capture_frame]  (keystone: WGC must return non-blank pixels)");
        let f1 = match p.capture_frame(None) {
            Ok(f) => {
                let blank = is_blank(&f.pixels);
                println!(
                    "  {}  {}x{}  blank={}",
                    if blank { "FAIL" } else { "PASS" },
                    f.width,
                    f.height,
                    blank
                );
                save("onbox_1_capture.png", f.width, f.height, &f.pixels);
                Some(f)
            }
            Err(e) => {
                println!("  FAIL  {e}");
                None
            }
        };

        println!("\n[send_key Text]");
        let typed = "hello from glass-windows";
        match p.send_key(&KeyEvent::Text(typed.into())) {
            Ok(()) => println!("  sent {typed:?}"),
            Err(e) => println!("  FAIL  {e}"),
        }
        std::thread::sleep(Duration::from_millis(900));
        if let (Some(f1), Ok(f2)) = (&f1, p.capture_frame(None)) {
            let ch = changed(&f1.pixels, &f2.pixels);
            println!(
                "  after-type changed_px = {ch}  ({})",
                if ch > 0 { "PASS text rendered" } else { "FAIL no change" }
            );
            save("onbox_2_typed.png", f2.width, f2.height, &f2.pixels);
        }

        println!("\n[list_windows]");
        match p.list_windows() {
            Ok(ws) => {
                println!("  {} window(s):", ws.len());
                for w in &ws {
                    println!(
                        "    id={:?} active={} title={:?} class={:?} geo={:?}",
                        w.id, w.active, w.title, w.class, w.geometry
                    );
                }
            }
            Err(e) => println!("  FAIL  {e}"),
        }

        println!("\n[window Move -> (140,140)]  (DWM-frame offset read-back)");
        match p.window(&WindowOp::Move { x: 140, y: 140 }) {
            Ok(g) => {
                let ok = (g.x - 140).abs() <= 2 && (g.y - 140).abs() <= 2;
                println!("  -> {g:?}  ({})", if ok { "PASS within 2px" } else { "CHECK offset" });
            }
            Err(e) => println!("  FAIL  {e}"),
        }
        println!("\n[window Resize -> 720x520]");
        match p.window(&WindowOp::Resize { width: 720, height: 520 }) {
            Ok(g) => {
                let ok = (g.width as i64 - 720).abs() <= 2 && (g.height as i64 - 520).abs() <= 2;
                println!("  -> {g:?}  ({})", if ok { "PASS within 2px" } else { "CHECK offset" });
            }
            Err(e) => println!("  FAIL  {e}"),
        }
        std::thread::sleep(Duration::from_millis(600));
        if let Ok(f) = p.capture_frame(None) {
            save("onbox_3_moved_resized.png", f.width, f.height, &f.pixels);
        }

        println!("\n[send_pointer Click (50,50)]");
        match p.send_pointer(&PointerEvent::Click { x: 50, y: 50, button: MouseButton::Left, count: 1, modifiers: vec![] }) {
            Ok(()) => println!("  sent"),
            Err(e) => println!("  FAIL  {e}"),
        }

        println!("\n[drain_logs]");
        let logs = p.drain_logs();
        println!("  {} line(s) (Notepad is a GUI app → 0 expected)", logs.len());

        println!("\n[stop_app — Job kill-tree]");
        let pid = p.app_pid();
        match p.stop_app() {
            Ok(()) => println!("  stopped (root pid was {pid:?})"),
            Err(e) => println!("  FAIL  {e}"),
        }
        std::thread::sleep(Duration::from_millis(900));
        if let Some(pid) = pid {
            match std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
            {
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout);
                    let alive = s.contains(&pid.to_string());
                    println!(
                        "  survivor check: {}",
                        if alive { "FAIL root pid still alive" } else { "PASS root pid gone" }
                    );
                }
                Err(e) => println!("  survivor check inconclusive: {e}"),
            }
        }
        println!("\n== done ==");
    }
}
