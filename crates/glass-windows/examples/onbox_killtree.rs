//! On-box validation: multi-process descendant-pid discovery + Job kill-tree, through the backend.
//!
//! Launches an ISOLATED Edge instance (a unique `--user-data-dir`, so it's our own child tree, not
//! the box's already-running background Edge) via `WindowsPlatform::start_app`, confirms discovery
//! finds the browser window and capture is non-blank, then `stop_app` and verifies the WHOLE
//! multi-process tree is gone (KILL_ON_JOB_CLOSE). Survivors are counted by the unique user-data-dir
//! marker so the box's other Edge processes don't confound the result.
//!
//! Windows-only; no-op elsewhere. Run in an interactive session via the scheduled-task bridge.

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox_killtree` example is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use glass_core::{AppSpec, Platform};
    use glass_windows::WindowsPlatform;
    use std::time::Duration;

    const EDGE: &str = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
    const UDD: &str = r"C:\Users\mpd\glass-kt-probe";
    const MARKER: &str = "glass-kt-probe";

    fn is_blank(px: &[u8]) -> bool {
        match px.chunks_exact(4).next() {
            Some(first) => px.chunks_exact(4).all(|c| c == first),
            None => true,
        }
    }

    /// Count msedge.exe processes belonging to OUR isolated instance (command line carries the
    /// unique user-data-dir marker), via CIM so the box's background Edge isn't counted.
    fn our_edge_count() -> i32 {
        let ps = format!(
            "@(Get-CimInstance Win32_Process -Filter \"Name='msedge.exe'\" | \
             Where-Object {{ $_.CommandLine -like '*{MARKER}*' }}).Count"
        );
        match std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(-1),
            Err(_) => -1,
        }
    }

    pub fn run() {
        // SAFETY: process-global DPI setting, first DPI-sensitive call; harmless if already set.
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        println!("== glass-windows multi-process discovery + Job kill-tree (isolated Edge) ==");
        let _ = std::fs::remove_dir_all(UDD); // start clean

        let mut p = match WindowsPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                println!("FATAL new(): {e}");
                return;
            }
        };
        let spec = AppSpec {
            build: None,
            run: vec![
                EDGE.to_string(),
                format!("--user-data-dir={UDD}"),
                "--no-first-run".to_string(),
                "--no-default-browser-check".to_string(),
                "--new-window".to_string(),
                "about:blank".to_string(),
            ],
            cwd: None,
            env: vec![],
            window_hint: None, // pid-set only: a class hint could match the box's background Edge
            timeout_ms: 25_000,
            sandbox: glass_core::SandboxLevel::Off,
        };

        println!("\n[start_app isolated Edge]");
        match p.start_app(&spec) {
            Ok(g) => println!("  PASS discovery -> {g:?}  (root pid {:?})", p.app_pid()),
            Err(e) => {
                println!("  FAIL discovery: {e}");
                println!("  (if AppExited: the launched process handed off + exited; see notes)");
                let _ = p.stop_app();
                let _ = std::fs::remove_dir_all(UDD);
                return;
            }
        }
        let root = p.app_pid();
        std::thread::sleep(Duration::from_secs(4)); // let the renderer/GPU/utility children spawn

        let before = our_edge_count();
        println!(
            "\n[process tree] our msedge.exe processes BEFORE stop: {before}  ({})",
            if before >= 2 { "PASS — a real multi-process tree" } else { "unexpected (<2)" }
        );

        println!("\n[capture_frame]");
        match p.capture_frame(None) {
            Ok(f) => println!(
                "  {}  {}x{}  blank={}",
                if is_blank(&f.pixels) { "FAIL" } else { "PASS" },
                f.width,
                f.height,
                is_blank(&f.pixels)
            ),
            Err(e) => println!("  FAIL: {e}"),
        }

        println!("\n[stop_app — Job kill-tree on the whole tree]");
        match p.stop_app() {
            Ok(()) => println!("  stopped (root pid was {root:?})"),
            Err(e) => println!("  FAIL: {e}"),
        }
        std::thread::sleep(Duration::from_secs(3)); // give the tree time to fully terminate

        let after = our_edge_count();
        println!(
            "  our msedge.exe processes AFTER stop: {after}  ({})",
            if after == 0 {
                "PASS — NO survivors (the whole tree died with the job)"
            } else {
                "FAIL — survivors escaped the job"
            }
        );

        let _ = std::fs::remove_dir_all(UDD);
        println!("\n== done ==");
    }
}
