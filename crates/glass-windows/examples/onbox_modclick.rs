//! On-box smoke for modifier-held click on Windows: launch Character Map, find the first
//! interactable element via UIA, and send a plain click, a Ctrl+click, and a Shift+click at its
//! center — confirming the modified-click `SendInput` batch (modifier-VK downs → mouse → ups)
//! submits cleanly on a real interactive desktop (a short send warns to stderr). The modifier
//! *delivery* itself is proven equivalent by the live X11/Wayland integration tests (which assert
//! `ctrl`→`state=4`) and the on-box-validated chord modifier-VK primitives this reuses.
//! Windows-only; a no-op elsewhere so the Linux dev box stays green.
//!   cargo run -p glass-windows --example onbox_modclick

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox_modclick` example is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use glass_a11y_windows::WindowsA11y;
    use glass_core::{
        Accessibility, AppSpec, AxContext, AxNode, Modifier, MouseButton, Platform, PointerEvent,
        WindowHint,
    };
    use glass_windows::WindowsPlatform;
    use std::time::Duration;

    /// First interactable node with on-screen bounds (pre-order).
    fn first_clickable<'a>(n: &'a AxNode, out: &mut Option<&'a AxNode>) {
        if out.is_none() && n.role.is_interactable() && n.bounds.is_some() {
            *out = Some(n);
        }
        for c in &n.children {
            first_clickable(c, out);
        }
    }

    pub fn run() {
        // SAFETY: process-global DPI awareness before any coords (the example has no manifest).
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        println!("== glass-windows on-box modifier-click smoke ==");

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
        let geo = match p.start_app(&spec) {
            Ok(g) => {
                println!("  start PASS  {g:?}  (pid {:?})", p.app_pid());
                g
            }
            Err(e) => {
                println!("  start FAIL  {e}");
                return;
            }
        };
        std::thread::sleep(Duration::from_millis(1200));

        let mut a11y = WindowsA11y::new();
        let ctx = AxContext { pids: p.app_pids(), window: geo.clone() };
        let tree = match a11y.snapshot(&ctx) {
            Ok(t) => t,
            Err(e) => {
                println!("  snapshot FAIL  {e}");
                let _ = p.stop_app();
                return;
            }
        };
        let mut hit = None;
        first_clickable(&tree.root, &mut hit);
        let Some(n) = hit else {
            println!("  FAIL  no interactable element in tree");
            let _ = p.stop_app();
            return;
        };
        let Some((cx, cy)) = n.bounds.and_then(|b| b.clamped_center(geo.width, geo.height)) else {
            println!("  FAIL  target has no clampable center");
            let _ = p.stop_app();
            return;
        };
        println!("  target #{} {:?} {:?} -> center ({cx},{cy})", n.id.0, n.role, n.name);

        for (label, mods) in [
            ("plain", Vec::new()),
            ("ctrl ", vec![Modifier::Control]),
            ("shift", vec![Modifier::Shift]),
        ] {
            match p.send_pointer(&PointerEvent::Click {
                x: cx,
                y: cy,
                button: MouseButton::Left,
                count: 1,
                modifiers: mods,
            }) {
                Ok(()) => println!("  {label}-click PASS (submitted; a short send would warn above)"),
                Err(e) => println!("  {label}-click FAIL {e}"),
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        let _ = p.stop_app();
        println!("== done ==");
    }
}
