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

/// How long the *first* snapshot of a session waits for `uiautomator` to become able to
/// dump: a device reaches `sys.boot_completed` — all the platform waits for before
/// reporting the app up — several seconds before the dump can serve one. Later snapshots
/// must not wait, or a caller like `wait_for_element`, which runs a snapshot per tick
/// inside its own budget, would be held long past it.
const DUMP_READY_TIMEOUT_MS: u64 = 30_000;
const DUMP_POLL_INTERVAL_MS: u64 = 1_000;

/// Runs one adb command and returns its `(stdout, stderr)` — the seam that lets the dump
/// sequence be driven by a fake instead of a device.
type AdbRunner<'a> = dyn FnMut(&[&str]) -> Result<(String, String)> + 'a;

/// Bind a runner to a real device.
pub(crate) fn adb_runner(adb: &Adb) -> impl FnMut(&[&str]) -> Result<(String, String)> + '_ {
    move |argv| adb.run_streams(argv.iter().copied())
}

/// One `uiautomator dump`, returning the XML it wrote.
///
/// `uiautomator dump` exits 0 even when it fails and reports the reason on stderr, so
/// neither its exit status nor its stdout can be trusted; the file it was asked to write
/// is the only reliable success signal. A stale file is removed first, best-effort, so a
/// previous run's tree does not stand in for one this dump never wrote.
pub(crate) fn dump_once(run: &mut AdbRunner<'_>, path: &str) -> Result<String> {
    let _ = run(&["shell", "rm", "-f", path]);
    let (_, stderr) = run(&["shell", "uiautomator", "dump", path])?;
    match run(&["shell", "cat", path]) {
        Ok((xml, _)) => Ok(xml),
        // The dump explained itself on stderr: that is why there is no file, and it names
        // the dump rather than the read that came up empty. Its stdout is never the reason
        // — it carries only the success line.
        Err(_) if !stderr.trim().is_empty() => Err(GlassError::AccessibilityUnavailable(format!(
            "uiautomator dump did not write {path}: {}",
            stderr.trim()
        ))),
        // A dump that said nothing leaves the read as the only evidence, and a read that
        // fails on its own is about the device rather than a dump yet to become possible.
        Err(e) => Err(e),
    }
}

/// Dump, retrying while `uiautomator` reports it cannot serve one yet, up to `budget`.
///
/// Only that one failure resolves by waiting: an adb or device error is returned at once,
/// so a device that has gone away is not retried for the whole budget.
fn dump_until_ready(
    run: &mut AdbRunner<'_>,
    path: &str,
    budget: Duration,
    interval: Duration,
) -> Result<String> {
    let deadline = Instant::now() + budget;
    loop {
        match dump_once(run, path) {
            Ok(xml) => return Ok(xml),
            Err(e) => {
                let retryable = matches!(e, GlassError::AccessibilityUnavailable(_));
                if !retryable || Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(interval);
            }
        }
    }
}

/// Reads the active window's accessibility tree via `uiautomator`.
pub struct AndroidA11y {
    adb: Adb,
    resolved: bool,
    /// Set once a dump has succeeded, after which snapshots stop waiting for readiness.
    warmed: bool,
}

impl AndroidA11y {
    pub fn new() -> Self {
        Self {
            adb: Adb::from_env(),
            resolved: false,
            warmed: false,
        }
    }

    /// Bind directly to an already-resolved (serial-bound) adb client. Used in production so
    /// the reader talks to the exact device the platform resolved, instead of re-resolving.
    pub fn for_adb(adb: Adb) -> Self {
        Self {
            adb,
            resolved: true,
            warmed: false,
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
        let budget = if self.warmed {
            Duration::ZERO
        } else {
            Duration::from_millis(DUMP_READY_TIMEOUT_MS)
        };
        let xml = dump_until_ready(
            &mut adb_runner(&adb),
            DUMP_PATH,
            budget,
            Duration::from_millis(DUMP_POLL_INTERVAL_MS),
        )?;
        self.warmed = true;
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
    use super::{dump_once, dump_until_ready, locate_editable_target};
    use glass_core::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree};
    use glass_core::{GlassError, Result, WindowGeometry};
    use std::time::Duration;

    const PATH: &str = "/sdcard/glass_dump.xml";
    const XML: &str = "<?xml version='1.0'?><hierarchy rotation=\"0\"></hierarchy>";

    /// What `uiautomator dump` writes to stderr on a device that has booted but whose
    /// accessibility bridge is not serving yet — captured from a cold emulator, where the
    /// dump also exits 0 and prints nothing on stdout.
    const NOT_READY: &str = "ERROR: null root node returned by UiTestAutomationBridge.";

    /// What `uiautomator dump` prints on stdout when it succeeds (typo upstream's).
    const DUMPED: &str = "UI hierchary dumped to: /sdcard/glass_dump.xml";

    /// The `cat` of a file the dump never wrote — the error the old code surfaced in place
    /// of the dump's own.
    fn read_err() -> GlassError {
        GlassError::Backend(format!(
            "`adb shell cat {PATH}` failed: cat: {PATH}: No such file"
        ))
    }

    /// An adb whose `uiautomator dump` fails as a cold device's does for `cold` attempts,
    /// then succeeds. Records every command it is given.
    fn fake(cold: usize) -> impl FnMut(&[&str]) -> Result<(String, String)> {
        let mut dumps = 0;
        let mut wrote = false;
        move |argv: &[&str]| match argv {
            ["shell", "rm", "-f", _] => {
                wrote = false;
                Ok((String::new(), String::new()))
            }
            ["shell", "uiautomator", "dump", _] => {
                dumps += 1;
                if dumps > cold {
                    wrote = true;
                    Ok((DUMPED.to_string(), String::new()))
                } else {
                    // Exit 0, nothing on stdout, the diagnosis on stderr.
                    Ok((String::new(), format!("{NOT_READY}\n")))
                }
            }
            ["shell", "cat", _] if wrote => Ok((XML.to_string(), String::new())),
            ["shell", "cat", _] => Err(read_err()),
            other => panic!("unexpected adb command: {other:?}"),
        }
    }

    #[test]
    fn dump_reports_the_dump_that_wrote_nothing_not_the_read_that_found_nothing() {
        let mut run = fake(1);
        let e = dump_once(&mut run, PATH).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains(&format!("uiautomator dump did not write {PATH}")),
            "must name the step that should have written the file: {msg}"
        );
        assert!(
            msg.contains(NOT_READY),
            "must quote uiautomator's own diagnosis: {msg}"
        );
        assert!(
            !msg.contains("cat:"),
            "must not send the reader to the missing file: {msg}"
        );
    }

    #[test]
    fn dump_clears_a_stale_file_before_dumping() {
        let mut seen: Vec<String> = Vec::new();
        let mut run = |argv: &[&str]| -> Result<(String, String)> {
            seen.push(argv.join(" "));
            match argv {
                ["shell", "cat", _] => Ok((XML.to_string(), String::new())),
                _ => Ok((String::new(), String::new())),
            }
        };
        dump_once(&mut run, PATH).unwrap();
        assert_eq!(
            seen,
            vec![
                format!("shell rm -f {PATH}"),
                format!("shell uiautomator dump {PATH}"),
                format!("shell cat {PATH}"),
            ],
            "a stale tree must not be able to stand in for a dump that never ran"
        );
    }

    #[test]
    fn a_read_failure_the_dump_did_not_explain_is_returned_as_it_stands() {
        // The dump succeeded and said so; the read then failed on its own. Blaming the dump
        // here would repeat the misattribution this fix exists to remove.
        let mut run = |argv: &[&str]| -> Result<(String, String)> {
            match argv {
                ["shell", "cat", _] => Err(read_err()),
                _ => Ok((DUMPED.to_string(), String::new())),
            }
        };
        let e = dump_once(&mut run, PATH).unwrap_err();
        assert!(matches!(e, GlassError::Backend(_)), "{e}");
        assert!(!e.to_string().contains("did not write"), "{e}");
    }

    #[test]
    fn a_dump_that_is_not_ready_yet_is_retried_until_it_is() {
        let mut run = fake(3);
        let xml = dump_until_ready(&mut run, PATH, Duration::from_secs(30), Duration::ZERO)
            .expect("a device that becomes ready within the budget must produce a tree");
        assert_eq!(xml, XML);
    }

    #[test]
    fn a_dump_that_never_becomes_ready_fails_with_the_last_reason() {
        let mut run = fake(usize::MAX);
        let e = dump_until_ready(&mut run, PATH, Duration::ZERO, Duration::ZERO).unwrap_err();
        assert!(e.to_string().contains(NOT_READY), "{e}");
    }

    #[test]
    fn an_adb_failure_is_not_retried() {
        // A device that has gone away will not come back by waiting, and a caller polling
        // with its own timeout must not be held past it.
        let mut attempts = 0;
        let mut run = |argv: &[&str]| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", _] => {
                    attempts += 1;
                    Err(GlassError::Backend(
                        "device 'emulator-5554' not found".into(),
                    ))
                }
                _ => Ok((String::new(), String::new())),
            }
        };
        let e =
            dump_until_ready(&mut run, PATH, Duration::from_secs(30), Duration::ZERO).unwrap_err();
        assert!(matches!(e, GlassError::Backend(_)), "{e}");
        assert_eq!(
            attempts, 1,
            "must not wait out the budget on a device error"
        );
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
