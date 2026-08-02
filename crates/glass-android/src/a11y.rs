//! `AndroidA11y` — the Android accessibility reader. Drives `uiautomator dump`
//! over adb and maps the result via `crate::axmap`. Resolves its own device
//! lazily, since the `Accessibility` trait is handed only an `AxContext`.

use std::time::{Duration, Instant};

use glass_core::accessibility::{Accessibility, AxContext, AxNode, AxTarget, AxTree};
use glass_core::{
    GlassError, KeyEvent, MouseButton, NOT_STARTED, PointerEvent, Result, TIMED_OUT,
    WindowGeometry, typed_clear_landed, typed_text_landed,
};

use crate::adb::{Adb, AdbOp};
use crate::axmap::build_tree;
use crate::input::{key_commands, pointer_commands};
use crate::target::{choose_serial, parse_devices};

const DUMP_PATH: &str = "/sdcard/glass_dump.xml";

/// How long the *first* snapshot of a session waits for `uiautomator` to become able to
/// dump: a device reaches `sys.boot_completed` — all the platform waits for before
/// reporting the app up — several seconds before the dump can serve one. Later snapshots
/// must not wait, or a caller like `wait_for_element`, which runs a snapshot per tick
/// inside its own budget, would be held long past it. This is time spent *retrying*; one
/// attempt costs `AdbOp::Dump`'s budget on top of it (see [`attempt_deadline`]), which a
/// warmed snapshot pays too.
const DUMP_READY_TIMEOUT_MS: u64 = 30_000;
const DUMP_POLL_INTERVAL_MS: u64 = 1_000;

/// Runs one adb command, no further out than `deadline`, and returns its `(stdout, stderr)` — the
/// seam that lets the dump sequence be driven by a fake instead of a device.
type AdbRunner<'a> = dyn FnMut(&[&str], Instant) -> Result<(String, String)> + 'a;

/// Bind a runner to a real device.
pub(crate) fn adb_runner(
    adb: &Adb,
) -> impl FnMut(&[&str], Instant) -> Result<(String, String)> + '_ {
    move |argv, deadline| adb.run_streams_until(argv.iter().copied(), deadline)
}

/// When one whole dump attempt — the stale-file removal, the dump, and the read of what it wrote —
/// must be done by.
///
/// The three share [`AdbOp::Dump`]'s budget — one snapshot's worth — with the removal keeping its
/// own `AdbOp::Shell` ceiling inside that. A step carrying only its own budget let an attempt cost
/// the sum of all three, 50s against the 10s a `glass_wait_for_element` asks for by default and
/// re-snapshots inside.
pub(crate) fn attempt_deadline() -> Instant {
    Instant::now() + AdbOp::Dump.budget()
}

/// One `uiautomator dump`, returning the XML it wrote. Every step is bounded by `deadline` — see
/// [`attempt_deadline`].
///
/// `uiautomator dump` exits 0 even when it fails and reports the reason on stderr, so
/// neither its exit status nor its stdout can be trusted; the file it was asked to write
/// is the only reliable success signal. A stale file is removed first, best-effort, so a
/// previous run's tree does not stand in for one this dump never wrote.
pub(crate) fn dump_once(run: &mut AdbRunner<'_>, path: &str, deadline: Instant) -> Result<String> {
    let _ = run(&["shell", "rm", "-f", path], deadline);
    let (_, stderr) = run(&["shell", "uiautomator", "dump", path], deadline)?;
    match run(&["shell", "cat", path], deadline) {
        Ok((xml, _)) => Ok(xml),
        // The dump explained itself on stderr: that is why there is no file, and it names
        // the dump rather than the read that came up empty. Its stdout is never the reason
        // — it carries only the success line.
        Err(e) if !stderr.trim().is_empty() && !bound_fired(&e) => {
            Err(GlassError::AccessibilityUnavailable(format!(
                "uiautomator dump did not write {path}: {}",
                stderr.trim()
            )))
        }
        // A dump that said nothing leaves the read as the only evidence, and a read that
        // fails on its own is about the device rather than a dump yet to become possible.
        Err(e) => Err(e),
    }
}

/// Whether an error is the attempt's own deadline firing rather than the device answering.
///
/// A read that ran out of the attempt's time never reached the device, so the dump's stderr is no
/// explanation for it — and that substitution is retryable, so the loop would go on retrying a
/// device that had answered. Matched on the phrases `glass_core` publishes for exactly this.
fn bound_fired(e: &GlassError) -> bool {
    let msg = e.to_string();
    msg.contains(TIMED_OUT) || msg.contains(NOT_STARTED)
}

/// Dump, retrying while `uiautomator` reports it cannot serve one yet, up to `budget`.
///
/// Only that one failure resolves by waiting: an adb or device error is returned at once,
/// so a device that has gone away is not retried for the whole budget.
///
/// Returns within `budget` plus one attempt — `AdbOp::Dump`'s budget, per [`attempt_deadline`].
/// `budget` bounds the retrying, waits between attempts included, and the last attempt it starts
/// still has a whole dump to make.
fn dump_until_ready(
    run: &mut AdbRunner<'_>,
    path: &str,
    budget: Duration,
    interval: Duration,
) -> Result<String> {
    let retry_until = Instant::now() + budget;
    loop {
        match dump_once(run, path, attempt_deadline()) {
            Ok(xml) => return Ok(xml),
            Err(e) => {
                let retryable = matches!(e, GlassError::AccessibilityUnavailable(_));
                let left = retry_until.saturating_duration_since(Instant::now());
                if !retryable || left.is_zero() {
                    return Err(e);
                }
                // Clamped, so the attempt this wait leads to still starts inside `budget` — an
                // unclamped wait would put a whole further attempt past it.
                std::thread::sleep(interval.min(left));
            }
        }
    }
}

/// Judge a typed `set_value` from a tree read back after it. `Ok(())` only when the field holds
/// exactly what was asked for, or — for a clear — reads back empty.
///
/// Exact match, not "changed from before": tap-and-type is not atomic, so a dropped key or an input
/// filter leaves the field holding something that is neither the request nor the old value, and
/// calling that success is the failure this check exists to prevent. `glass_core::typed_text_landed`
/// and `glass_core::typed_clear_landed` carry the rules and the cost of them.
///
/// The element is re-resolved by its pre-order id, which the write can perturb if the tap changes
/// what is on screen. Raising the soft keyboard does not: measured on the dogfood AVD with
/// `mInputShown=true`, `uiautomator dump` emits the focused window only and no IME window, so the
/// keyboard cannot shift ids. A tap that navigates — Settings' search entry opens a different
/// window — does shift them, so a mismatch is [`GlassError::AxElementChanged`] ("re-snapshot")
/// rather than a claim about the write.
///
/// Role and name are checked but bounds deliberately are not, unlike the pre-write
/// [`locate_editable_target`]: the IME reflows the layout under the field it is typing into, so a
/// moved-but-correct element is the normal case here.
///
/// The node must also still be editable, which excludes a non-editable neighbour that inherited the
/// id — but not a second nameless editable, since an editable's name comes from `content-desc` alone
/// and `AxTarget::matches` compares `None` to `None`. The exact-value requirement is what covers
/// that case.
fn verify_write(after_tree: &AxTree, target: &AxTarget, text: &str) -> Result<()> {
    let Some(node) = after_tree.find(target.id) else {
        // A tree cut short by the node cap explains an absent element better than "it moved" does.
        return Err(match &after_tree.truncated {
            // `Truncation::notice()` is written to close a rendered outline — a leading ellipsis
            // and its own pixel-fallback advice — so this states the cap itself instead.
            Some(t) => GlassError::AccessibilityUnavailable(format!(
                "set_value: the text was typed, but the read-back could not find element {} \
                 because the tree was truncated at {} {}; re-snapshot rather than retyping",
                target.id.0,
                t.limit_value,
                t.limit.label(),
            )),
            None => GlassError::AxElementChanged(target.id.0),
        });
    };
    if !target.matches(node.role, node.name.as_deref()) || !node.states.editable {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    let landed = if text.is_empty() {
        typed_clear_landed(node.value.as_deref())
    } else {
        typed_text_landed(node.value.as_deref(), text)
    };
    if landed {
        Ok(())
    } else {
        Err(GlassError::AxValueNotApplied(target.id.0))
    }
}

/// How many times to read the element back before reporting the write as not applied. A landed
/// write confirms on the first read and pays for one; the retries exist for a field that commits a
/// frame or two later — a Compose recompose, a debounced handler — which the on-device service
/// reader polls two whole seconds for.
const VERIFY_ATTEMPTS: usize = 3;

/// How long to let the toolkit commit typed text before reading it back. Generous relative to a
/// keystroke and small next to the `uiautomator dump` that follows it.
const VERIFY_SETTLE_MS: u64 = 300;

/// Readiness budget for one post-write read-back. Do NOT reuse [`DUMP_READY_TIMEOUT_MS`] here: that
/// is the once-per-session cold-boot budget, and each read-back also pays for the attempt it ends
/// with, so at [`VERIFY_ATTEMPTS`] attempts it would let a routine write hold the single-threaded
/// tool loop for two and a half minutes. What this waits out is an IME animation, which takes
/// hundreds of milliseconds.
const VERIFY_READY_BUDGET_MS: u64 = 2_000;

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

    /// One dump, retrying a not-ready device for up to `budget`.
    ///
    /// Split out of [`Accessibility::snapshot`] so a caller that knows the UI is mid-flux can ask
    /// for retries even on a warmed reader: `snapshot` gives a warmed reader no budget, which is
    /// right for a `wait_for_element` tick and wrong immediately after typing.
    fn snapshot_with_budget(&mut self, ctx: &AxContext, budget: Duration) -> Result<AxTree> {
        let window = ctx.window.clone();
        let adb = self.ensure_adb()?;
        let xml = dump_until_ready(
            &mut adb_runner(&adb),
            DUMP_PATH,
            budget,
            Duration::from_millis(DUMP_POLL_INTERVAL_MS),
        )?;
        self.warmed = true;
        build_tree(&xml, &window, ctx.limits)
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

/// Find `target.id` and reject a tree that drifted under it — shared by [`editable_target`]
/// and the service reader's `invoke`, which needs the same rejection without the editable check.
pub(crate) fn fingerprinted<'a>(tree: &'a AxTree, target: &AxTarget) -> Result<&'a AxNode> {
    let node = tree
        .find(target.id)
        .ok_or(GlassError::AxElementNotFound(target.id.0))?;
    if !target.matches(node.role, node.name.as_deref())
        || !target.bounds_consistent(node.bounds, 8)
        || !target.value_consistent(node.value.as_deref())
    {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    Ok(node)
}

/// Re-resolve `target` in an already-numbered `tree` and return the node only if it is still the
/// element that was addressed and still editable. Errors specifically when the target is gone
/// (`AxElementNotFound`), has drifted in role/name/bounds/value (`AxElementChanged`), or is not
/// editable (`AxElementNotEditable`).
///
/// Both Android readers' `set_value` guards route through this — the check that stops a write
/// landing on whatever inherited the id between the snapshot the caller read and the one the write
/// acts on. A recycled `RecyclerView` row is the motivating case for comparing `value` too — see
/// `AxTarget::value`'s doc. Pure (no device I/O), so it is testable without a device.
pub(crate) fn editable_target<'a>(tree: &'a AxTree, target: &AxTarget) -> Result<&'a AxNode> {
    let node = fingerprinted(tree, target)?;
    if !node.states.editable {
        return Err(GlassError::AxElementNotEditable(target.id.0));
    }
    Ok(node)
}

/// [`editable_target`], plus the window-relative tap point for editing it — the extra step the
/// `uiautomator` reader needs, because it edits by tapping where the on-device service can act on
/// the node directly. Errors `AxElementNotClickable` when the element has no on-screen center.
fn locate_editable_target(
    tree: &AxTree,
    target: &AxTarget,
    window: &WindowGeometry,
) -> Result<(i32, i32)> {
    editable_target(tree, target)?
        .bounds
        .and_then(|b| b.clamped_center(window.width, window.height))
        .ok_or(GlassError::AxElementNotClickable(target.id.0))
}

impl Accessibility for AndroidA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let budget = if self.warmed {
            Duration::ZERO
        } else {
            Duration::from_millis(DUMP_READY_TIMEOUT_MS)
        };
        self.snapshot_with_budget(ctx, budget)
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

        // Each read-back is a whole `uiautomator dump` — measured at ~2.3s on the dogfood AVD — so
        // this reads once and retries only the one verdict a later read can overturn.
        //
        // A failure of this read is NOT a failure of the write — the field has already been cleared
        // and typed into — so it says so, because a caller that retries blindly types twice. The
        // readiness budget is non-zero even on a warmed reader: the IME and any suggestion strip are
        // still animating, which is exactly when a dump comes back not-ready.
        let mut last = None;
        for _ in 0..VERIFY_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(VERIFY_SETTLE_MS));
            let mut after = self
                .snapshot_with_budget(ctx, Duration::from_millis(VERIFY_READY_BUDGET_MS))
                .map_err(|e| {
                    GlassError::AccessibilityUnavailable(format!(
                        "set_value: the text was typed, but reading the element back failed: {e}; \
                         re-snapshot to see whether it landed rather than retyping"
                    ))
                })?;
            after.assign_ids();
            match verify_write(&after, target, text) {
                Ok(()) => return Ok(()),
                // Only a not-applied verdict can change on a later read: drift and truncation are
                // structural, and re-dumping for them costs seconds to reach the same answer.
                Err(e @ GlassError::AxValueNotApplied(_)) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or(GlassError::AxValueNotApplied(target.id.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dump_once, dump_until_ready, editable_target, locate_editable_target, verify_write,
    };
    use crate::adb::AdbOp;
    use glass_core::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree};
    use glass_core::{GlassError, Result, WindowGeometry};
    use std::time::{Duration, Instant};

    /// A deadline no test reaches, for the cases that are not about the bound.
    fn ample() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

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
    fn fake(cold: usize) -> impl FnMut(&[&str], Instant) -> Result<(String, String)> {
        let mut dumps = 0;
        let mut wrote = false;
        move |argv: &[&str], _deadline: Instant| match argv {
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
        let e = dump_once(&mut run, PATH, ample()).unwrap_err();
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
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
            seen.push(argv.join(" "));
            match argv {
                ["shell", "cat", _] => Ok((XML.to_string(), String::new())),
                _ => Ok((String::new(), String::new())),
            }
        };
        dump_once(&mut run, PATH, ample()).unwrap();
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
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "cat", _] => Err(read_err()),
                _ => Ok((DUMPED.to_string(), String::new())),
            }
        };
        let e = dump_once(&mut run, PATH, ample()).unwrap_err();
        assert!(matches!(e, GlassError::Backend(_)), "{e}");
        assert!(!e.to_string().contains("did not write"), "{e}");
    }

    #[test]
    fn the_three_steps_of_one_dump_share_one_deadline_worth_a_single_dump() {
        // The defect this fixes: each step carrying its own budget let one attempt cost their sum
        // (10s + 20s + 20s), which the loop's deadline — checked only between attempts — could not
        // see. The lower bound is the other half: an attempt gets a whole dump's worth even when,
        // as here, the loop has no budget to retry with.
        let mut seen: Vec<Instant> = Vec::new();
        let mut run = |argv: &[&str], deadline: Instant| -> Result<(String, String)> {
            seen.push(deadline);
            match argv {
                ["shell", "cat", _] => Ok((XML.to_string(), String::new())),
                _ => Ok((String::new(), String::new())),
            }
        };
        let started = Instant::now();
        dump_until_ready(&mut run, PATH, Duration::ZERO, Duration::ZERO).unwrap();

        assert_eq!(seen.len(), 3, "one attempt is three adb calls");
        assert!(
            seen.iter().all(|d| *d == seen[0]),
            "the steps of one dump must share one deadline"
        );
        assert!(
            seen[0] >= started + AdbOp::Dump.budget(),
            "an attempt must get a whole dump's worth, not what is left of the loop's budget"
        );
        // Slack for the test's own work between `started` and the attempt.
        assert!(
            seen[0] <= started + AdbOp::Dump.budget() + Duration::from_secs(1),
            "an attempt must not be allowed the sum of its steps' budgets"
        );
    }

    #[test]
    fn a_retried_attempt_gets_a_fresh_deadline() {
        let mut seen: Vec<Instant> = Vec::new();
        let mut cold = fake(1);
        let mut run = |argv: &[&str], deadline: Instant| -> Result<(String, String)> {
            seen.push(deadline);
            cold(argv, deadline)
        };
        let xml = dump_until_ready(
            &mut run,
            PATH,
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .expect("the second attempt succeeds");

        assert_eq!(xml, XML);
        assert_eq!(seen.len(), 6, "two attempts of three adb calls");
        assert!(
            seen[3] > seen[0],
            "a retry must be given its own deadline, not the spent remains of the first attempt's"
        );
    }

    #[test]
    fn the_wait_between_attempts_cannot_push_one_past_the_retry_budget() {
        // An interval far longer than the budget is where an unclamped wait would spend a further
        // 2s and then start an attempt licensed to run 20s past the ceiling.
        let budget = Duration::from_millis(50);
        let mut seen: Vec<Instant> = Vec::new();
        let mut cold = fake(usize::MAX);
        let mut run = |argv: &[&str], deadline: Instant| -> Result<(String, String)> {
            seen.push(deadline);
            cold(argv, deadline)
        };
        let started = Instant::now();
        dump_until_ready(&mut run, PATH, budget, Duration::from_secs(2))
            .expect_err("a device that never becomes ready");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the wait ran past the budget it was supposed to fit inside: {:?}",
            started.elapsed()
        );
        // Slack for the scheduler: a sleep clamped to the budget still wakes a little after it.
        let ceiling = started + budget + AdbOp::Dump.budget() + Duration::from_millis(250);
        assert!(
            seen.iter().all(|d| *d <= ceiling),
            "no call may outlive the loop's budget plus one attempt"
        );
    }

    #[test]
    fn a_read_that_ran_out_of_the_attempts_time_is_not_reported_as_a_dump_that_wrote_nothing() {
        // Sharing one deadline across the three steps is what makes this reachable: an earlier slow
        // step can leave the read none.
        //
        // The error is the one `glass_core` really produces for a spent deadline, not a fixture
        // repeating wording this crate does not own. Nothing is spawned on that path, so it needs
        // no real command.
        let spent = glass_core::run_bounded_until(
            &mut std::process::Command::new("adb"),
            Duration::from_secs(10),
            Instant::now(),
            "adb:uiautomator dump",
        )
        .expect_err("a spent deadline starts nothing")
        .to_string();

        let mut attempts = 0;
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", _] => {
                    attempts += 1;
                    Ok((String::new(), format!("{NOT_READY}\n")))
                }
                ["shell", "cat", _] => Err(GlassError::Backend(spent.clone())),
                _ => Ok((String::new(), String::new())),
            }
        };
        let e =
            dump_until_ready(&mut run, PATH, Duration::from_secs(30), Duration::ZERO).unwrap_err();

        let msg = e.to_string();
        assert!(
            msg.contains(glass_core::NOT_STARTED),
            "the step that ran out of time must be the one reported: {msg}"
        );
        assert!(
            !msg.contains("did not write"),
            "a read that never ran is no evidence about the dump: {msg}"
        );
        assert_eq!(
            attempts, 1,
            "an attempt that ran out of time is not a device that is not ready yet"
        );
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
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
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
            description: None,
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

    /// The same single-node tree, holding `value` — what a read-back sees.
    fn tree_holding(value: Option<&str>) -> AxTree {
        let mut t = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true);
        t.root.value = value.map(Into::into);
        t.assign_ids();
        t
    }

    /// Like `tree`, but also holding `value` — see `editable_target`'s doc for why that's part of
    /// the fingerprint too. Always at `BOUNDS`, always editable.
    fn tree_with_value(role: AxRole, name: Option<&str>, value: Option<&str>) -> AxTree {
        let mut t = tree(role, name, Some(BOUNDS), true);
        t.root.value = value.map(Into::into);
        t.assign_ids();
        t
    }

    fn target(id: u32, name: Option<&str>, bounds: Option<AxRect>) -> AxTarget {
        AxTarget {
            id: AxNodeId(id),
            role: AxRole::TextField,
            name: name.map(Into::into),
            bounds,
            value: None,
        }
    }

    #[test]
    fn a_write_that_landed_is_reported_as_success() {
        let after = tree_holding(Some("world"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(verify_write(&after, &t, "world").is_ok());
    }

    #[test]
    fn clearing_a_field_is_a_write_that_can_succeed() {
        // An empty field reports no value at all, so a rule that read `None` as "unknown" made
        // `set_value(id, "")` fail every time — which is what it did before this was fixed.
        let after = tree_holding(None);
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(verify_write(&after, &t, "").is_ok());
    }

    #[test]
    fn a_cleared_field_reporting_its_hint_reads_as_not_applied() {
        // The cost of judging a clear by "reads back empty", pinned as a decision: this device does
        // empty the field and `uiautomator` does report the hint as its text. Accepting "the value
        // changed" instead would accept a clear that never fired on any field that reformats when it
        // takes focus, which is the false success this check exists to prevent.
        let after = tree_holding(Some("Search settings"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, ""),
            Err(GlassError::AxValueNotApplied(0))
        ));
    }

    #[test]
    fn a_field_that_never_changed_is_not_a_successful_write() {
        // The case `verify_write`'s doc is about: the field still holds the old text.
        let after = tree_holding(Some("hello"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AxValueNotApplied(0))
        ));
    }

    #[test]
    fn a_partly_typed_write_is_not_a_successful_write() {
        // The reason the rule is an exact match; `typed_text_landed` carries the argument.
        let after = tree_holding(Some("worl"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AxValueNotApplied(0))
        ));
    }

    #[test]
    fn a_field_that_reformats_what_it_was_given_reports_not_applied() {
        // The cost of the strictness, pinned so it is a decision rather than a surprise: a mask that
        // turns "1234567890" into "(123) 456-7890" reads as not applied. An agent can re-read the
        // tree and see the value; a false success would have it asserting against the wrong text.
        let after = tree_holding(Some("(123) 456-7890"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, "1234567890"),
            Err(GlassError::AxValueNotApplied(0))
        ));
    }

    #[test]
    fn a_target_that_moved_is_reported_as_changed_not_as_a_failed_write() {
        // The write may well have landed — on something that then moved. Saying "not applied"
        // would send an agent to retype into whatever now sits at that id.
        let after = tree_holding(Some("world"));
        let t = target(0, Some("A different field"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AxElementChanged(0))
        ));
    }

    #[test]
    fn a_non_editable_node_at_the_id_is_reported_as_changed() {
        // Excludes a non-editable neighbour that inherited the id — a label, or a container the
        // walk shifted into place.
        let mut after = tree_holding(Some("world"));
        after.root.states.editable = false;
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AxElementChanged(0))
        ));
    }

    #[test]
    fn a_target_missing_from_the_read_back_is_reported_as_changed() {
        let after = tree_holding(Some("world"));
        let t = target(7, Some("Search"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AxElementChanged(7))
        ));
    }

    #[test]
    fn a_truncated_read_back_says_so_rather_than_blaming_drift() {
        // The tree carries the reason the element is absent; reporting "it moved" would throw away
        // the one fact that explains it.
        let mut after = tree_holding(Some("world"));
        after.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 1,
            nodes_walked: 1,
        });
        let t = target(7, Some("Search"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AccessibilityUnavailable(_))
        ));
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
    fn a_target_whose_value_moved_is_rejected() {
        // The recycled-row case `editable_target`'s doc names: role, name and rect match here too.
        let tree = tree_with_value(AxRole::TextField, Some("row_title"), Some("Zara"));
        let target = AxTarget {
            id: AxNodeId(0),
            role: AxRole::TextField,
            name: Some("row_title".into()),
            bounds: tree.root.bounds,
            value: Some("Alice".into()),
        };
        assert!(matches!(
            editable_target(&tree, &target),
            Err(GlassError::AxElementChanged(0))
        ));
    }

    #[test]
    fn a_target_with_no_captured_value_still_passes() {
        // The `None`-must-not-gate case from the comment on `editable_target`'s value check.
        let tree = tree_with_value(AxRole::TextField, Some("row_title"), Some("Alice"));
        let target = AxTarget {
            id: AxNodeId(0),
            role: AxRole::TextField,
            name: Some("row_title".into()),
            bounds: tree.root.bounds,
            value: None,
        };
        assert!(editable_target(&tree, &target).is_ok());
    }

    #[test]
    fn a_target_whose_value_vanished_is_rejected() {
        // Android reports an emptied field as no value at all, so this is the shape a row that
        // recycled from filled to empty arrives in — a real change, not a missing observation.
        let tree = tree_with_value(AxRole::TextField, Some("row_title"), None);
        let target = AxTarget {
            id: AxNodeId(0),
            role: AxRole::TextField,
            name: Some("row_title".into()),
            bounds: tree.root.bounds,
            value: Some("Alice".into()),
        };
        assert!(matches!(
            editable_target(&tree, &target),
            Err(GlassError::AxElementChanged(0))
        ));
    }

    #[test]
    fn a_target_whose_value_still_matches_passes() {
        // Every other target helper here captures `None`, so "reject whenever a value was
        // captured" would pass all of them too — this is the one case that actually needs the
        // comparison, not just the presence check.
        let tree = tree_with_value(AxRole::TextField, Some("row_title"), Some("Alice"));
        let target = AxTarget {
            id: AxNodeId(0),
            role: AxRole::TextField,
            name: Some("row_title".into()),
            bounds: tree.root.bounds,
            value: Some("Alice".into()),
        };
        assert!(editable_target(&tree, &target).is_ok());
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
