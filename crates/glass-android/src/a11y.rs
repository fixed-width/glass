//! `AndroidA11y` — the Android accessibility reader. Drives `uiautomator dump`
//! over adb and maps the result via `crate::axmap`. Resolves its own device
//! lazily, since the `Accessibility` trait is handed only an `AxContext`.

use std::time::{Duration, Instant};

use glass_core::accessibility::{Accessibility, AxContext, AxTarget, AxTree};
use glass_core::{GlassError, KeyEvent, MouseButton, PointerEvent, Result, WindowGeometry};

use crate::adb::Adb;
use crate::axmap::build_tree;
use crate::input::{key_commands, pointer_commands};
use crate::target::{choose_serial, parse_devices};

const DUMP_PATH: &str = "/sdcard/glass_dump.xml";

/// How long a snapshot keeps retrying the dump. A device reports `sys.boot_completed` —
/// all the platform waits for before reporting the app up — several seconds before
/// `uiautomator` can serve a dump, so a snapshot taken right after a cold boot would
/// otherwise fail on a device that is merely still starting.
const DUMP_READY_TIMEOUT_MS: u64 = 30_000;
const DUMP_POLL_INTERVAL_MS: u64 = 500;

/// One `uiautomator dump`, returning the XML it wrote.
///
/// `uiautomator dump` exits 0 even when it fails and reports the reason on stderr, so
/// neither its exit status nor its stdout can be trusted; the file it was asked to write
/// — removed first, so a previous run's tree cannot stand in for it — is the only
/// reliable success signal.
pub(crate) fn dump_once(adb: &Adb, path: &str) -> Result<String> {
    let _ = adb.run(["shell", "rm", "-f", path]);
    let (out, err) = adb.run_streams(["shell", "uiautomator", "dump", path])?;
    adb.run(["shell", "cat", path])
        .map_err(|read_err| dump_failed(path, &out, &err, &read_err))
}

/// The error for a dump that wrote no file: it names the dump rather than the read that
/// came up empty, and quotes uiautomator's own diagnosis in preference to ours. Pure, so
/// the cold-device signature is tested without a device.
fn dump_failed(path: &str, stdout: &str, stderr: &str, read_err: &GlassError) -> GlassError {
    let why = [stderr, stdout]
        .into_iter()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map_or_else(|| read_err.to_string(), str::to_string);
    GlassError::AccessibilityUnavailable(format!("uiautomator dump did not write {path}: {why}"))
}

/// Reads the active window's accessibility tree via `uiautomator`.
pub struct AndroidA11y {
    adb: Adb,
    resolved: bool,
}

impl AndroidA11y {
    pub fn new() -> Self {
        Self {
            adb: Adb::from_env(),
            resolved: false,
        }
    }

    /// Bind directly to an already-resolved (serial-bound) adb client. Used in production so
    /// the reader talks to the exact device the platform resolved, instead of re-resolving.
    pub fn for_adb(adb: Adb) -> Self {
        Self {
            adb,
            resolved: true,
        }
    }

    /// Bind the adb client to a device serial on first use (lazy).
    fn ensure_adb(&mut self) -> Result<Adb> {
        if !self.resolved {
            let listing = self.adb.run(["devices"])?;
            let online: Vec<_> = parse_devices(&listing)
                .into_iter()
                .filter(|d| d.state == "device")
                .collect();
            let serial = choose_serial(
                std::env::var("GLASS_ANDROID_SERIAL").ok().as_deref(),
                &online,
            )?;
            self.adb = self.adb.with_serial(serial);
            self.resolved = true;
        }
        Ok(self.adb.clone())
    }
}

impl Default for AndroidA11y {
    fn default() -> Self {
        Self::new()
    }
}

/// Locate `target` in an already-numbered `tree` and return the window-relative tap point for
/// editing it. Errors specifically when the target is gone (`AxElementNotFound`), has drifted in
/// role/name/bounds (`AxElementChanged`), is not editable (`AxElementNotEditable`), or has no
/// clickable on-screen center (`AxElementNotClickable`). Pure (no device I/O) so `set_value`'s
/// re-validation — the guard that stops it typing into the wrong element after a re-snapshot — is
/// testable without a device.
fn locate_editable_target(
    tree: &AxTree,
    target: &AxTarget,
    window: &WindowGeometry,
) -> Result<(i32, i32)> {
    let node = tree
        .find(target.id)
        .ok_or(GlassError::AxElementNotFound(target.id.0))?;
    if !target.matches(node.role, node.name.as_deref()) || !target.bounds_consistent(node.bounds, 8)
    {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    if !node.states.editable {
        return Err(GlassError::AxElementNotEditable(target.id.0));
    }
    node.bounds
        .and_then(|b| b.clamped_center(window.width, window.height))
        .ok_or(GlassError::AxElementNotClickable(target.id.0))
}

impl Accessibility for AndroidA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let window = ctx.window.clone();
        let adb = self.ensure_adb()?;
        let deadline = Instant::now() + Duration::from_millis(DUMP_READY_TIMEOUT_MS);
        let xml = loop {
            match dump_once(&adb, DUMP_PATH) {
                Ok(xml) => break xml,
                Err(e) if Instant::now() >= deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(DUMP_POLL_INTERVAL_MS)),
            }
        };
        build_tree(&xml, &window, ctx.limits)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let window = ctx.window.clone();
        // Re-snapshot and number nodes to locate the target by its pre-order id.
        let mut tree = self.snapshot(ctx)?;
        tree.assign_ids();
        let (cx, cy) = locate_editable_target(&tree, target, &window)?;

        let adb = self.ensure_adb()?;
        // Tap to focus, select-all, delete, type — reusing the P2 input builders.
        let tap = PointerEvent::Click {
            x: cx,
            y: cy,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        };
        for argv in pointer_commands(&window, &tap) {
            adb.run(argv.iter().map(String::as_str))?;
        }
        for ev in [
            KeyEvent::Chord("ctrl+a".into()),
            KeyEvent::Chord("BackSpace".into()),
            KeyEvent::Text(text.to_string()),
        ] {
            for argv in key_commands(&ev)? {
                adb.run(argv.iter().map(String::as_str))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{dump_failed, locate_editable_target};
    use glass_core::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree};
    use glass_core::{GlassError, WindowGeometry};

    /// What `uiautomator dump` writes to stderr on a device that has booted but whose
    /// accessibility bridge is not serving yet — captured from a cold emulator.
    const NOT_READY: &str = "ERROR: null root node returned by UiTestAutomationBridge.";

    /// What the read of the unwritten file reports, and what the old code surfaced.
    fn read_err() -> GlassError {
        GlassError::Backend(
            "`adb -s emulator-5554 shell cat /sdcard/glass_dump.xml` failed: \
             cat: /sdcard/glass_dump.xml: No such file or directory"
                .into(),
        )
    }

    #[test]
    fn dump_failure_names_the_dump_not_the_read() {
        let e = dump_failed("/sdcard/glass_dump.xml", "", NOT_READY, &read_err());
        let msg = e.to_string();
        assert!(
            msg.contains("uiautomator dump did not write /sdcard/glass_dump.xml"),
            "must name the step that should have written the file: {msg}"
        );
        assert!(
            !msg.contains("cat:"),
            "must not send the reader to the missing file instead of the dump: {msg}"
        );
    }

    #[test]
    fn dump_failure_quotes_uiautomators_own_diagnosis() {
        let e = dump_failed("/sdcard/glass_dump.xml", "", NOT_READY, &read_err());
        assert!(e.to_string().contains(NOT_READY), "{e}");
        assert!(matches!(e, GlassError::AccessibilityUnavailable(_)));
    }

    #[test]
    fn dump_failure_prefers_stderr_over_stdout() {
        // The failing dump leaves stdout empty, but a version that also chatters on stdout
        // must not bury the diagnosis.
        let e = dump_failed(
            "/p.xml",
            "UI hierchary dumped to: /p.xml",
            NOT_READY,
            &read_err(),
        );
        let msg = e.to_string();
        assert!(msg.contains(NOT_READY), "{msg}");
        assert!(
            !msg.contains("hierchary"),
            "the success chatter is not the reason: {msg}"
        );
    }

    #[test]
    fn dump_failure_falls_back_to_the_read_error_when_the_dump_said_nothing() {
        // A dump that says nothing on either stream leaves the read as the only evidence,
        // so a silent failure still reports something a reader can act on.
        let e = dump_failed("/p.xml", "  ", "\n", &read_err());
        assert!(e.to_string().contains("No such file or directory"), "{e}");
    }

    const WIN: WindowGeometry = WindowGeometry {
        x: 0,
        y: 0,
        width: 1080,
        height: 2400,
    };
    const BOUNDS: AxRect = AxRect {
        x: 100,
        y: 200,
        width: 400,
        height: 80,
    };

    /// A single-node tree (`root` = the target after `assign_ids` sets id 0).
    fn tree(role: AxRole, name: Option<&str>, bounds: Option<AxRect>, editable: bool) -> AxTree {
        let root = AxNode {
            id: AxNodeId(0),
            role,
            raw_role: String::new(),
            name: name.map(Into::into),
            value: None,
            states: AxStates {
                editable,
                ..Default::default()
            },
            bounds,
            children: vec![],
        };
        let mut t = AxTree::new(root);
        t.assign_ids();
        t
    }

    fn target(id: u32, name: Option<&str>, bounds: Option<AxRect>) -> AxTarget {
        AxTarget {
            id: AxNodeId(id),
            role: AxRole::TextField,
            name: name.map(Into::into),
            bounds,
        }
    }

    #[test]
    fn returns_the_visible_center_for_a_matching_editable_target() {
        let t = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true);
        // Center of [100,500] x [200,280].
        assert_eq!(
            locate_editable_target(&t, &target(0, Some("Search"), Some(BOUNDS)), &WIN).unwrap(),
            (300, 240)
        );
    }

    #[test]
    fn absent_id_is_element_not_found() {
        let t = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true);
        assert!(matches!(
            locate_editable_target(&t, &target(9, Some("Search"), Some(BOUNDS)), &WIN),
            Err(GlassError::AxElementNotFound(9))
        ));
    }

    #[test]
    fn drifted_name_is_element_changed() {
        // Same id lands on a different-named element (tree drift) — must refuse, not overwrite.
        let t = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true);
        assert!(matches!(
            locate_editable_target(&t, &target(0, Some("Other"), Some(BOUNDS)), &WIN),
            Err(GlassError::AxElementChanged(0))
        ));
    }

    #[test]
    fn drifted_bounds_is_element_changed() {
        let t = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true);
        let moved = AxRect { x: 700, ..BOUNDS };
        assert!(matches!(
            locate_editable_target(&t, &target(0, Some("Search"), Some(moved)), &WIN),
            Err(GlassError::AxElementChanged(0))
        ));
    }

    #[test]
    fn non_editable_target_is_element_not_editable() {
        let t = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), false);
        assert!(matches!(
            locate_editable_target(&t, &target(0, Some("Search"), Some(BOUNDS)), &WIN),
            Err(GlassError::AxElementNotEditable(0))
        ));
    }

    #[test]
    fn zero_area_bounds_is_element_not_clickable() {
        let flat = AxRect { width: 0, ..BOUNDS };
        let t = tree(AxRole::TextField, Some("Search"), Some(flat), true);
        assert!(matches!(
            locate_editable_target(&t, &target(0, Some("Search"), Some(flat)), &WIN),
            Err(GlassError::AxElementNotClickable(0))
        ));
    }
}
