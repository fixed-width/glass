//! On-box validation of glass-a11y-windows `set_value` (UIA `ValuePattern`).
//!
//! Launches Character Map, snapshots its UIA tree, finds an editable Edit field, sets its value
//! by `#id` through `WindowsA11y::set_value` (the same path `glass_set_value` uses), re-snapshots
//! to confirm the value changed, and confirms a non-editable element (a Button) returns
//! `AxElementNotEditable`. Windows-only; a no-op elsewhere so the Linux dev box stays green. Run
//! in an interactive session via the scheduled-task bridge:
//!   cargo run -p glass-windows --example onbox_a11y_setvalue

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox_a11y_setvalue` example is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use glass_a11y_windows::WindowsA11y;
    use glass_core::{
        Accessibility, AppSpec, AxContext, AxNode, AxRole, AxTarget, GlassError, Platform,
        WindowHint,
    };
    use glass_windows::WindowsPlatform;
    use std::time::Duration;

    /// First pre-order node of the given role.
    fn first_role<'a>(n: &'a AxNode, role: AxRole, out: &mut Option<&'a AxNode>) {
        if out.is_none() && n.role == role {
            *out = Some(n);
        }
        for c in &n.children {
            first_role(c, role, out);
        }
    }

    pub fn run() {
        // SAFETY: process-global DPI setting before any capture/coords (the example carries no
        // manifest); UIA bounds must be physical pixels on a scaled monitor.
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        println!("== glass-a11y-windows on-box set_value validation ==");

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

        let mut a11y = WindowsA11y::new();
        let ctx = AxContext { pids: p.app_pids(), window: geo.clone() };

        // Snapshot 1: locate an editable Edit field (AxRole::TextField).
        println!("\n[snapshot 1]");
        let tree = match a11y.snapshot(&ctx) {
            Ok(t) => {
                println!("  PASS  {} nodes", t.count);
                t
            }
            Err(e) => {
                println!("  FAIL snapshot: {e}");
                let _ = p.stop_app();
                return;
            }
        };
        let mut field = None;
        first_role(&tree.root, AxRole::TextField, &mut field);
        let Some(field) = field else {
            println!("  FAIL  no TextField (Edit) in tree:\n{}", tree.to_outline());
            let _ = p.stop_app();
            return;
        };
        let target =
            AxTarget { id: field.id, role: field.role, name: field.name.clone(), bounds: field.bounds };
        println!(
            "\n[set_value]  field #{} {:?} {:?}  value-before = {:?}",
            field.id.0, field.role, field.name, field.value
        );

        const NEW: &str = "GLASSVALUE";
        match a11y.set_value(&ctx, &target, NEW) {
            Ok(()) => println!("  set_value Ok"),
            Err(e) => {
                println!("  FAIL set_value: {e}");
                let _ = p.stop_app();
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(500));

        // Snapshot 2: confirm the field's value changed.
        println!("\n[snapshot 2 — confirm]");
        match a11y.snapshot(&ctx) {
            Ok(t2) => {
                let mut f2 = None;
                first_role(&t2.root, AxRole::TextField, &mut f2);
                // charmap's "Characters to copy" Edit keeps a trailing CR; compare trimmed.
                match f2.and_then(|n| n.value.as_deref()) {
                    Some(v) if v.trim_end() == NEW => {
                        println!("  PASS  value-after = {v:?}  (set_value worked)")
                    }
                    other => println!("  FAIL  value-after = {other:?}  (expected {NEW:?})"),
                }
            }
            Err(e) => println!("  FAIL re-snapshot: {e}"),
        }

        // Not-editable: set_value on a Button must error AxElementNotEditable.
        println!("\n[not-editable guard]");
        let mut button = None;
        first_role(&tree.root, AxRole::Button, &mut button);
        match button {
            Some(b) => {
                let bt = AxTarget { id: b.id, role: b.role, name: b.name.clone(), bounds: b.bounds };
                match a11y.set_value(&ctx, &bt, "x") {
                    Err(GlassError::AxElementNotEditable(_)) => {
                        println!("  PASS  button #{} -> AxElementNotEditable", b.id.0)
                    }
                    other => {
                        println!("  FAIL  button set_value -> {other:?} (expected AxElementNotEditable)")
                    }
                }
            }
            None => println!("  (no Button found for the not-editable check)"),
        }

        println!("\n[stop_app]");
        let _ = p.stop_app();
        println!("\n== done ==");
    }
}
