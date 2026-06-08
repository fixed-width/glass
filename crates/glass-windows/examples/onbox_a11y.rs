//! On-box validation of the Windows UI Automation accessibility reader (glass-a11y-windows).
//!
//! Launches a control-rich classic app (Character Map), snapshots its UIA tree via `WindowsA11y`
//! (correlated by the launched pid), prints the normalized outline + interactable count, and clicks
//! the first interactable element by its bounds' clamped center — exercising the same
//! `AxTree` → click-by-element path the `glass_click_element` MCP tool uses. Windows-only; a no-op
//! elsewhere so `cargo test`/`clippy --all-targets` stay green on the Linux dev box. Run in an
//! interactive session via the scheduled-task bridge:
//!   cargo run -p glass-windows --example onbox_a11y

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox_a11y` example is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use glass_a11y_windows::WindowsA11y;
    use glass_core::{
        Accessibility, AppSpec, AxContext, AxNode, MouseButton, Platform, PointerEvent, WindowHint,
    };
    use glass_windows::WindowsPlatform;
    use std::time::Duration;

    /// Total nodes and how many are interactable (drive the Set-of-Mark numbering).
    fn counts(n: &AxNode, total: &mut usize, interactable: &mut usize) {
        *total += 1;
        if n.role.is_interactable() {
            *interactable += 1;
        }
        for c in &n.children {
            counts(c, total, interactable);
        }
    }

    /// First interactable node that has on-screen bounds (pre-order), for the click test.
    fn first_clickable<'a>(n: &'a AxNode, out: &mut Option<&'a AxNode>) {
        if out.is_none() && n.role.is_interactable() && n.bounds.is_some() {
            *out = Some(n);
        }
        for c in &n.children {
            first_clickable(c, out);
        }
    }

    pub fn run() {
        // SAFETY: process-global DPI setting before any capture/coords (the example carries no
        // manifest); UIA bounds and click coords must be physical pixels on a scaled monitor.
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        println!("== glass-a11y-windows on-box validation (UIA reader) ==");

        let mut p = match WindowsPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                println!("FATAL WindowsPlatform::new: {e}");
                return;
            }
        };
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
        let geo = match p.start_app(&spec) {
            Ok(g) => {
                println!("  PASS  geometry = {g:?}  (pid {:?})", p.app_pid());
                g
            }
            Err(e) => {
                println!("  FAIL  {e}");
                return;
            }
        };
        std::thread::sleep(Duration::from_millis(1500));

        println!("\n[glass_a11y_snapshot via WindowsA11y]  (the keystone: a live UIA tree)");
        let mut a11y = WindowsA11y::new();
        let ctx = AxContext { pids: p.app_pids(), window: geo.clone() };
        match a11y.snapshot(&ctx) {
            Ok(tree) => {
                let (mut total, mut inter) = (0usize, 0usize);
                counts(&tree.root, &mut total, &mut inter);
                println!(
                    "  PASS  {} nodes ({} interactable)  root={:?} {:?}",
                    tree.count, inter, tree.root.role, tree.root.name
                );
                println!("  (count={}, hand-count={})", tree.count, total);
                println!("\n---- outline ----\n{}", tree.to_outline());

                println!("[glass_click_element path] click first interactable element by bounds");
                let mut hit = None;
                first_clickable(&tree.root, &mut hit);
                match hit {
                    Some(n) => match n.bounds.and_then(|b| b.clamped_center(geo.width, geo.height)) {
                        Some((cx, cy)) => {
                            println!(
                                "  #{} {:?} {:?} -> center ({cx},{cy})",
                                n.id.0, n.role, n.name
                            );
                            match p.send_pointer(&PointerEvent::Click {
                                x: cx,
                                y: cy,
                                button: MouseButton::Left,
                                count: 1,
                                modifiers: vec![],
                            }) {
                                Ok(()) => println!("  PASS click sent (verify the element reacted)"),
                                Err(e) => println!("  FAIL send_pointer: {e}"),
                            }
                        }
                        None => println!("  (first interactable had no clampable center)"),
                    },
                    None => println!("  FAIL no interactable element with bounds in the tree"),
                }
            }
            Err(e) => println!("  FAIL snapshot: {e}"),
        }

        println!("\n[stop_app]");
        let _ = p.stop_app();
        println!("\n== done ==");
    }
}
