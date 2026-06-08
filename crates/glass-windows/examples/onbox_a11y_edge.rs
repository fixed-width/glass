//! On-box stress test for the Windows UIA reader on a MULTI-PROCESS app (isolated Edge).
//!
//! Validates the two paths charmap (single-process, small) couldn't exercise:
//!  - the **geometry fallback** in `find_app_window` — Edge's top-level window is owned by a
//!    descendant process, so the launched-root-pid exact match misses and the reader must fall back
//!    to matching `ctx.window` geometry (reported via a foreground-window-pid diagnostic), and
//!  - **snapshot latency + the node cap** on a real multi-hundred-node tree with pattern-probe gating.
//!
//! Windows-only; no-op elsewhere. Run in an interactive session via the scheduled-task bridge.

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox_a11y_edge` example is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use glass_a11y_windows::WindowsA11y;
    use glass_core::{Accessibility, AppSpec, AxContext, AxNode, Platform};
    use glass_windows::WindowsPlatform;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    const EDGE: &str = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
    const UDD: &str = r"C:\Users\mpd\glass-a11y-edge";

    fn counts(n: &AxNode, total: &mut usize, interactable: &mut usize) {
        *total += 1;
        if n.role.is_interactable() {
            *interactable += 1;
        }
        for c in &n.children {
            counts(c, total, interactable);
        }
    }

    pub fn run() {
        // SAFETY: process-global DPI setting before any coords (no manifest on the example).
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        println!("== glass-a11y-windows multi-process stress test (isolated Edge) ==");
        let _ = std::fs::remove_dir_all(UDD);

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
            window_hint: None,
            timeout_ms: 25_000,
            sandbox: glass_core::SandboxLevel::Off,
        };

        println!("\n[start_app isolated Edge]");
        let geo = match p.start_app(&spec) {
            Ok(g) => {
                println!("  PASS  geometry = {g:?}  (launched root pid {:?})", p.app_pid());
                g
            }
            Err(e) => {
                println!("  FAIL  {e}");
                let _ = std::fs::remove_dir_all(UDD);
                return;
            }
        };
        let root = p.app_pid();
        std::thread::sleep(Duration::from_secs(4)); // let the renderer/GPU children spawn

        // Diagnostic: is the top-level window owned by the launched ROOT pid, or a DESCENDANT?
        // If descendant, the a11y reader's exact-pid match misses and the geometry fallback is what
        // recovers the window.
        // SAFETY: GetForegroundWindow + GetWindowThreadProcessId are pure queries.
        let fg_pid = unsafe {
            let mut pid = 0u32;
            let _ = GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
            pid
        };
        println!(
            "\n[pid diagnostic] root={root:?}  foreground-window owner={fg_pid}  ->  {}",
            if Some(fg_pid) == root {
                "window owned by ROOT (exact pid match)"
            } else {
                "window owned by a DESCENDANT -> a11y reader must use the GEOMETRY FALLBACK"
            }
        );

        println!("\n[glass_a11y_snapshot on the multi-process tree]");
        let mut a11y = WindowsA11y::new();
        let ctx = AxContext { pids: p.app_pids(), window: geo.clone() };
        let t0 = Instant::now();
        match a11y.snapshot(&ctx) {
            Ok(tree) => {
                let dt = t0.elapsed();
                let (mut total, mut inter) = (0usize, 0usize);
                counts(&tree.root, &mut total, &mut inter);
                println!(
                    "  PASS  {} nodes ({} interactable) in {:?}  root={:?} {:?}  [cap: {}]",
                    tree.count,
                    inter,
                    dt,
                    tree.root.role,
                    tree.root.name,
                    if tree.count >= 1500 { "HIT MAX_NODES" } else { "under" }
                );
                println!("  (latency {:?} vs 10s timeout — pattern-gating + caps keep it bounded)", dt);
                // a small sample of the tree (first 25 lines) so we can eyeball role/name fidelity
                let outline = tree.to_outline();
                let sample: String = outline.lines().take(25).collect::<Vec<_>>().join("\n");
                println!("\n---- outline (first 25 of {} lines) ----\n{sample}", outline.lines().count());
            }
            Err(e) => println!("  FAIL snapshot: {e}"),
        }

        println!("\n[stop_app — kill the Edge tree]");
        let _ = p.stop_app();
        let _ = std::fs::remove_dir_all(UDD);
        println!("\n== done ==");
    }
}
