//! `AndroidA11y` — the Android accessibility reader. Drives `uiautomator dump`
//! over adb and maps the result via `crate::axmap`. Resolves its own device
//! lazily, since the `Accessibility` trait is handed only an `AxContext`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use glass_core::accessibility::{Accessibility, AxContext, AxNode, AxTarget, AxTree};
use glass_core::{
    GlassError, KeyEvent, MouseButton, NOT_STARTED, PointerEvent, Result, TIMED_OUT,
    WindowGeometry, typed_clear_landed, typed_text_landed,
};

use crate::adb::{Adb, AdbOp};
use crate::axmap::build_tree;
use crate::input::{key_commands, pointer_commands};
use crate::target::{choose_serial, parse_devices};

/// What every dump path of this reader starts with; [`attempt_path`] completes it.
const DUMP_PREFIX: &str = "/sdcard/glass_dump";

/// The device path for one dump attempt — used by no other attempt, and by no other process.
///
/// An attempt killed at its deadline reaps the local adb client only; the `uiautomator dump` it
/// started still writes whenever it finishes, and on a shared path that write could answer a later
/// attempt with nothing marking the tree as old.
///
/// `prefix` must be a literal a device shell needs no quoting for: `adb shell` passes it on
/// unescaped.
fn attempt_path(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}_{}_{}.xml",
        process_tag(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Identifies this process among any others dumping to the same device. The time is of the first
/// dump, not of process start.
fn process_tag() -> &'static str {
    static TAG: OnceLock<String> = OnceLock::new();
    TAG.get_or_init(|| {
        // A fixed fallback would give every host with a pre-epoch clock the same tag; the error
        // carries the distance the other way.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|e| e.duration());
        format!("{}_{}", std::process::id(), now.as_nanos())
    })
    .as_str()
}

/// What the *first* snapshot of a session may spend waiting for `uiautomator` to become able to
/// dump: a device reaches `sys.boot_completed` — all the platform waits for before reporting the
/// app up — several seconds before the dump can serve one. Later snapshots must not wait like
/// this, or a caller like `wait_for_element`, which runs a snapshot per tick inside its own
/// budget, would be held long past it.
const COLD_BOUND: RetryBound = RetryBound {
    least: 2,
    then_within: Duration::from_millis(30_000),
};
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

/// When one whole dump attempt — the dump, the read of what it wrote, and the removal of the file
/// it read — must be done by.
///
/// The three share [`AdbOp::Dump`]'s budget — one snapshot's worth — with the removal keeping its
/// own `AdbOp::Shell` ceiling inside that. A step carrying only its own budget let an attempt cost
/// the sum of all three, 50s against the 10s a `glass_wait_for_element` asks for by default and
/// re-snapshots inside.
pub(crate) fn attempt_deadline() -> Instant {
    Instant::now() + AdbOp::Dump.budget()
}

/// What one dump attempt settled, for a loop deciding whether another would help.
///
/// The judgement is made here, where the failure is diagnosed, rather than inferred by the caller
/// from which error variant escaped: `uiautomator` crashing without a word arrived as
/// `GlassError::Backend`, and the `matches!` gate on `AccessibilityUnavailable` that refused to
/// retry it was repaired by restating the error rather than by fixing the gate (glass#341).
pub(crate) enum Attempt {
    Dumped(String),
    /// The device cannot serve a dump *yet* — waiting is what resolves it.
    NotReady(GlassError),
    /// Waiting cannot help: adb is gone, the device is wedged, or the attempt's deadline fired.
    Fatal(GlassError),
}

/// One `uiautomator dump`, returning the XML it wrote. Every step is bounded by `deadline` — see
/// [`attempt_deadline`].
///
/// `uiautomator dump` fails in two shapes: it exits 0 with the reason on stderr, or it crashes and
/// exits non-zero with stderr empty, its trace going to logcat instead (glass#341). Neither its
/// exit status nor its stdout can be trusted; the file it was asked to write
/// is the only reliable success signal. That file is this attempt's alone — see
/// [`attempt_path`], which `prefix` names the family for — so no other dump can stand in for one
/// this attempt never wrote.
///
/// The removal is last and best-effort, and names one path: a concurrent attempt's file is not
/// exposed to it, and housekeeping cannot spend the deadline the read needs.
pub(crate) fn dump_once(run: &mut AdbRunner<'_>, prefix: &str, deadline: Instant) -> Attempt {
    let path = attempt_path(prefix);
    let stderr = match run(&["shell", "uiautomator", "dump", &path], deadline) {
        Ok((_, stderr)) => stderr,
        Err(e) if died_unexplained(&e) => {
            // The crash is raised after the file is opened, so this attempt can own one. Retried,
            // each crash would strand another.
            let _ = run(&["shell", "rm", "-f", &path], deadline);
            return Attempt::NotReady(GlassError::AccessibilityUnavailable(format!(
                "uiautomator dump exited without writing {path} and without saying why; \
                 its reason, if any, is in logcat"
            )));
        }
        Err(e) => return Attempt::Fatal(e),
    };
    let read = run(&["shell", "cat", &path], deadline);
    let _ = run(&["shell", "rm", "-f", &path], deadline);
    match read {
        Ok((xml, _)) => Attempt::Dumped(xml),
        // The dump explained itself on stderr: that is why there is no file, and it names
        // the dump rather than the read that came up empty. Its stdout is never the reason
        // — it carries only the success line.
        Err(e) if !stderr.trim().is_empty() && !bound_fired(&e) => {
            Attempt::NotReady(GlassError::AccessibilityUnavailable(format!(
                "uiautomator dump did not write {path}: {}",
                stderr.trim()
            )))
        }
        // A dump that said nothing leaves the read as the only evidence, and a read that
        // fails on its own is about the device rather than a dump yet to become possible.
        Err(e) => Attempt::Fatal(e),
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

/// Whether a failed dump gave no reason of its own — the mark of a `uiautomator` that crashed.
///
/// It dies with a `NullPointerException` walking a tree that is still changing, exiting non-zero
/// with an empty stderr because the trace goes to logcat via `AndroidRuntime` (glass#341). That
/// resolves by waiting. adb's own failures — a device that is gone, a wedged server — always carry
/// a reason and do not.
fn died_unexplained(e: &GlassError) -> bool {
    !bound_fired(e) && e.to_string().trim_end().ends_with("failed:")
}

/// How long a readiness wait may retry for, and how many attempts it owes regardless.
///
/// A wall-clock budget alone cannot express "try twice": one attempt may cost up to
/// [`AdbOp::Dump`]'s whole budget, so a budget shorter than two of those can expire before a second
/// attempt ever starts. That is what a 2s budget did against attempts measured 3.5s apart — a retry
/// budget that never retried (glass#338).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetryBound {
    /// Attempts owed however long each one takes; 1 means no retry at all.
    least: u32,
    /// Once `least` is met, keep retrying while this much wall-clock remains.
    then_within: Duration,
}

impl RetryBound {
    /// Exactly one attempt.
    const ONCE: Self = Self {
        least: 1,
        then_within: Duration::ZERO,
    };
}

/// Dump, retrying while `uiautomator` cannot serve one yet, within `bound`.
///
/// Only [`Attempt::NotReady`] is retried, so a device that has gone away is reported at once
/// rather than waited on.
///
/// Returns after `bound.least` attempts plus however many more start while `bound.then_within`
/// remains — each costing up to `AdbOp::Dump`'s budget, per [`attempt_deadline`].
fn dump_until_ready(
    run: &mut AdbRunner<'_>,
    prefix: &str,
    bound: RetryBound,
    interval: Duration,
) -> Result<String> {
    let retry_until = Instant::now() + bound.then_within;
    let mut owed = bound.least.max(1);
    loop {
        match dump_once(run, prefix, attempt_deadline()) {
            Attempt::Dumped(xml) => return Ok(xml),
            Attempt::Fatal(e) => return Err(e),
            Attempt::NotReady(e) => {
                owed = owed.saturating_sub(1);
                let left = retry_until.saturating_duration_since(Instant::now());
                if owed == 0 && left.is_zero() {
                    return Err(e);
                }
                // Clamped, so an attempt the ceiling still governs starts inside it — unclamped, a
                // whole further attempt would land past the bound. An owed attempt waits only what
                // is left, which a spent ceiling makes zero.
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
            None => drifted(after_tree, target),
        });
    };
    if !target.matches(node.role, node.name.as_deref()) || !node.states.editable {
        return Err(drifted(after_tree, target));
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

/// Readiness bound for one post-write read-back.
///
/// The second attempt is owed rather than merely budgeted for: what it waits out is the
/// accessibility bridge finishing registration, ~300ms, but a loaded device spends seconds per
/// attempt, so the 2s budget this replaces was reliably gone before a second one could start
/// (glass#338).
///
/// Do NOT widen this to [`COLD_BOUND`]: at [`VERIFY_ATTEMPTS`] reads it would let a routine write
/// hold the single-threaded tool loop for minutes.
const VERIFY_BOUND: RetryBound = RetryBound {
    least: 2,
    then_within: Duration::ZERO,
};

/// Ceiling on the whole read-back phase, checked between attempts so it is shared across
/// [`VERIFY_ATTEMPTS`] rather than multiplied by them. A phase that runs over stops after the
/// attempt in flight, which [`VERIFY_BOUND`] still owes its second dump.
const VERIFY_PHASE_BUDGET_MS: u64 = 20_000;

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

    /// One dump, retrying a not-ready device within `bound`.
    ///
    /// Split out of [`Accessibility::snapshot`] so a caller that knows the UI is mid-flux can ask
    /// for retries even on a warmed reader: `snapshot` gives a warmed reader one attempt, which is
    /// right for a `wait_for_element` tick and wrong immediately after typing.
    fn snapshot_within(&mut self, ctx: &AxContext, bound: RetryBound) -> Result<AxTree> {
        let window = ctx.window.clone();
        let adb = self.ensure_adb()?;
        let xml = dump_until_ready(
            &mut adb_runner(&adb),
            DUMP_PREFIX,
            bound,
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

/// Whether any node in `tree` still presents as `target` — same role and name, wherever it sits.
/// Bounds and value are excluded: both move under a live app without the control going anywhere.
fn still_on_screen(tree: &AxTree, target: &AxTarget) -> bool {
    fn walk(node: &AxNode, target: &AxTarget) -> bool {
        target.matches(node.role, node.name.as_deref())
            || node.children.iter().any(|c| walk(c, target))
    }
    walk(&tree.root, target)
}

/// Which of the two disagreements a tree that no longer agrees with `target` has.
///
/// `AxElementChanged` sends the reader looking for where the element went, which is worth doing
/// only while it is still somewhere; nothing carrying its role and name means the screen was
/// replaced or the app that drew it restarted (glass#323).
///
/// Both Android readers get this. Do not tighten [`still_on_screen`] to include bounds or value:
/// `a11y_service`'s relaxation needs `AxElementChanged` in every case it can relax.
fn drifted(tree: &AxTree, target: &AxTarget) -> GlassError {
    if still_on_screen(tree, target) {
        GlassError::AxElementChanged(target.id.0)
    } else {
        GlassError::AxElementGone(target.id.0)
    }
}

/// Find `target.id` and reject a tree that drifted under it — shared by [`editable_target`]
/// and the service reader's `invoke`, which needs the same rejection without the editable check.
///
/// An id that resolves to nothing stays [`GlassError::AxElementNotFound`]; [`drifted`] classifies
/// only an id occupied by something unrelated.
pub(crate) fn fingerprinted<'a>(tree: &'a AxTree, target: &AxTarget) -> Result<&'a AxNode> {
    let node = tree
        .find(target.id)
        .ok_or(GlassError::AxElementNotFound(target.id.0))?;
    if !target.matches(node.role, node.name.as_deref())
        || !target.bounds_consistent(node.bounds, 8)
        || !target.value_consistent(node.value.as_deref())
    {
        return Err(drifted(tree, target));
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
        let bound = if self.warmed {
            // One attempt, because this reader cannot see the caller's deadline: `wait_for_element`
            // re-snapshots on its own schedule, and a second attempt here could cost it another
            // whole `AdbOp::Dump` budget past the timeout it was given.
            RetryBound::ONCE
        } else {
            COLD_BOUND
        };
        self.snapshot_within(ctx, bound)
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
        // and typed into — so it says so, because a caller that retries blindly types twice. Each
        // read retries a not-ready device even on a warmed reader: the IME and any suggestion strip
        // are still animating, which is exactly when a dump comes back not-ready.
        let phase_ends = Instant::now() + Duration::from_millis(VERIFY_PHASE_BUDGET_MS);
        let mut last = None;
        for _ in 0..VERIFY_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(VERIFY_SETTLE_MS));
            let mut after = self.snapshot_within(ctx, VERIFY_BOUND).map_err(|e| {
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
            if Instant::now() >= phase_ends {
                break;
            }
        }
        Err(last.unwrap_or(GlassError::AxValueNotApplied(target.id.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Attempt, RetryBound, dump_once, dump_until_ready, editable_target, locate_editable_target,
        verify_write,
    };
    use crate::adb::AdbOp;
    use glass_core::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree};
    use glass_core::{GlassError, Result, WindowGeometry};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// A deadline no test reaches, for the cases that are not about the bound.
    fn ample() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// An attempt as a plain `Result`, for the tests about what a dump reports rather than about
    /// whether waiting would help.
    fn settled(a: Attempt) -> Result<String> {
        match a {
            Attempt::Dumped(xml) => Ok(xml),
            Attempt::NotReady(e) | Attempt::Fatal(e) => Err(e),
        }
    }

    /// Retry for a whole cold-boot wait, for the tests about which failures are retried at all.
    fn patient() -> RetryBound {
        RetryBound {
            least: 1,
            then_within: Duration::from_secs(30),
        }
    }

    /// What a call site passes; each attempt extends it with an id of its own.
    const PREFIX: &str = "/sdcard/glass_dump";
    const XML: &str = "<?xml version='1.0'?><hierarchy rotation=\"0\"></hierarchy>";
    /// A tree from an earlier moment; it parses exactly as [`XML`] does, and only the test can
    /// tell them apart.
    const STALE_XML: &str = "<?xml version='1.0'?><hierarchy rotation=\"1\"></hierarchy>";

    /// What `uiautomator dump` writes to stderr on a device that has booted but whose
    /// accessibility bridge is not serving yet — captured from a cold emulator, where the
    /// dump also exits 0 and prints nothing on stdout.
    const NOT_READY: &str = "ERROR: null root node returned by UiTestAutomationBridge.";

    /// What `uiautomator dump` prints on stdout when it succeeds (typo upstream's).
    const DUMPED: &str = "UI hierchary dumped to: /sdcard/glass_dump_1234_9_0.xml";

    /// The `cat` of a file the dump never wrote — the error the old code surfaced in place
    /// of the dump's own.
    fn read_err(path: &str) -> GlassError {
        GlassError::Backend(format!(
            "`adb shell cat {path}` failed: cat: {path}: No such file"
        ))
    }

    /// What `Adb` raises for the crash [`died_unexplained`] names: non-zero exit, empty stderr.
    fn crash_err(path: &str) -> GlassError {
        GlassError::Backend(format!("`adb shell uiautomator dump {path}` failed: "))
    }

    /// An adb whose `uiautomator dump` crashes for `crashes` attempts, then succeeds.
    fn fake_crashing(crashes: usize) -> impl FnMut(&[&str], Instant) -> Result<(String, String)> {
        let mut dumps = 0;
        move |argv: &[&str], _deadline: Instant| match argv {
            ["shell", "uiautomator", "dump", path] => {
                dumps += 1;
                if dumps > crashes {
                    Ok((DUMPED.to_string(), String::new()))
                } else {
                    Err(crash_err(path))
                }
            }
            ["shell", "cat", _] if dumps > crashes => Ok((XML.to_string(), String::new())),
            ["shell", "cat", path] => Err(read_err(path)),
            ["shell", "rm", "-f", _] => Ok((String::new(), String::new())),
            other => panic!("unexpected adb command: {other:?}"),
        }
    }

    /// An adb whose `uiautomator dump` fails as a cold device's does for `cold` attempts,
    /// then succeeds.
    fn fake(cold: usize) -> impl FnMut(&[&str], Instant) -> Result<(String, String)> {
        let mut dumps = 0;
        move |argv: &[&str], _deadline: Instant| match argv {
            ["shell", "uiautomator", "dump", _] => {
                dumps += 1;
                if dumps > cold {
                    Ok((DUMPED.to_string(), String::new()))
                } else {
                    // Exit 0, nothing on stdout, the diagnosis on stderr.
                    Ok((String::new(), format!("{NOT_READY}\n")))
                }
            }
            ["shell", "cat", _] if dumps > cold => Ok((XML.to_string(), String::new())),
            ["shell", "cat", path] => Err(read_err(path)),
            ["shell", "rm", "-f", _] => Ok((String::new(), String::new())),
            other => panic!("unexpected adb command: {other:?}"),
        }
    }

    /// glass#338: a readiness budget shorter than one attempt is not a retry budget.
    ///
    /// The CI device spent ~3.5s per `uiautomator` attempt against a 2s budget, so the loop used
    /// the whole budget inside its first attempt and reported not-ready without trying again —
    /// while the bridge registration it was waiting on had finished in ~300ms.
    #[test]
    fn a_second_attempt_is_owed_even_when_the_first_outlasts_the_wall_clock_budget() {
        let mut dumps = 0;
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", _] => {
                    dumps += 1;
                    // Costs more than the whole budget below — the condition under test.
                    std::thread::sleep(Duration::from_millis(30));
                    if dumps > 1 {
                        Ok((DUMPED.to_string(), String::new()))
                    } else {
                        Ok((String::new(), format!("{NOT_READY}\n")))
                    }
                }
                ["shell", "cat", _] if dumps > 1 => Ok((XML.to_string(), String::new())),
                ["shell", "cat", path] => Err(read_err(path)),
                ["shell", "rm", "-f", _] => Ok((String::new(), String::new())),
                other => panic!("unexpected adb command: {other:?}"),
            }
        };
        let xml = dump_until_ready(
            &mut run,
            PREFIX,
            RetryBound {
                least: 2,
                then_within: Duration::from_millis(10),
            },
            Duration::ZERO,
        )
        .expect("the second attempt is owed however long the first took");

        assert_eq!(xml, XML);
        assert_eq!(dumps, 2, "exactly the attempt that was owed");
    }

    /// Retryability is the attempt's own verdict, not a guess from the error variant it carries:
    /// both of these are `GlassError::Backend` underneath.
    #[test]
    fn an_attempt_says_whether_waiting_could_help_rather_than_leaving_it_to_be_inferred() {
        let mut crashed = |argv: &[&str], _d: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", path] => Err(crash_err(path)),
                _ => Ok((String::new(), String::new())),
            }
        };
        assert!(matches!(
            dump_once(&mut crashed, PREFIX, ample()),
            Attempt::NotReady(_)
        ));

        let mut gone = |argv: &[&str], _d: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", _] => Err(GlassError::Backend(
                    "`adb shell uiautomator dump` failed: device offline".into(),
                )),
                _ => Ok((String::new(), String::new())),
            }
        };
        assert!(matches!(
            dump_once(&mut gone, PREFIX, ample()),
            Attempt::Fatal(_)
        ));
    }

    #[test]
    fn a_dump_abandoned_at_the_deadline_cannot_answer_a_later_attempt() {
        // Killing the attempt reaps the local adb client; the dump it asked for keeps running on
        // the device and writes whenever it finishes — here, after the next attempt has written
        // its own file and is about to read it.
        let mut files = HashMap::new();
        let mut abandoned: Option<String> = None;
        let mut dumps = 0;
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "rm", "-f", path] => {
                    files.remove(*path);
                    Ok((String::new(), String::new()))
                }
                ["shell", "uiautomator", "dump", path] => {
                    dumps += 1;
                    if dumps == 1 {
                        abandoned = Some((*path).to_string());
                        return Err(GlassError::Backend(format!(
                            "`adb shell uiautomator dump` {}",
                            glass_core::TIMED_OUT
                        )));
                    }
                    files.insert((*path).to_string(), XML.to_string());
                    Ok((DUMPED.to_string(), String::new()))
                }
                ["shell", "cat", path] => {
                    if let Some(p) = abandoned.take() {
                        files.insert(p, STALE_XML.to_string());
                    }
                    match files.get(*path) {
                        Some(xml) => Ok((xml.clone(), String::new())),
                        None => Err(read_err(path)),
                    }
                }
                other => panic!("unexpected adb command: {other:?}"),
            }
        };

        settled(dump_once(&mut run, PREFIX, ample()))
            .expect_err("the attempt whose client was killed");
        let xml = settled(dump_once(&mut run, PREFIX, ample()))
            .expect("the next attempt dumps for itself");

        assert_eq!(
            xml, XML,
            "the abandoned dump's tree was served as this attempt's"
        );
    }

    #[test]
    fn dump_reports_the_dump_that_wrote_nothing_not_the_read_that_found_nothing() {
        let mut dumped = String::new();
        let mut cold = fake(1);
        let e = {
            let mut run = |argv: &[&str], deadline: Instant| {
                if let ["shell", "uiautomator", "dump", path] = argv {
                    dumped = (*path).to_string();
                }
                cold(argv, deadline)
            };
            settled(dump_once(&mut run, PREFIX, ample())).unwrap_err()
        };

        let msg = e.to_string();
        assert!(
            msg.contains(&format!("uiautomator dump did not write {dumped}")),
            "must name the file the reader can go looking for: {msg}"
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

    /// A device that keeps the files it is given.
    fn fake_device(
        files: &mut HashMap<String, String>,
    ) -> impl FnMut(&[&str], Instant) -> Result<(String, String)> {
        move |argv: &[&str], _deadline: Instant| match argv {
            ["shell", "rm", "-f", path] => {
                files.remove(*path);
                Ok((String::new(), String::new()))
            }
            ["shell", "uiautomator", "dump", path] => {
                files.insert((*path).to_string(), XML.to_string());
                Ok((DUMPED.to_string(), String::new()))
            }
            ["shell", "cat", path] => match files.get(*path) {
                Some(xml) => Ok((xml.clone(), String::new())),
                None => Err(read_err(path)),
            },
            other => panic!("unexpected adb command: {other:?}"),
        }
    }

    #[test]
    fn an_attempt_takes_its_file_with_it() {
        let mut files = HashMap::new();
        let mut run = fake_device(&mut files);
        settled(dump_once(&mut run, PREFIX, ample())).unwrap();
        settled(dump_once(&mut run, PREFIX, ample())).unwrap();
        drop(run);

        assert!(
            files.is_empty(),
            "a file per snapshot fills the device it is dumping: {files:?}"
        );
    }

    #[test]
    fn an_attempt_removes_no_file_but_its_own() {
        // A concurrent attempt — a second glass, or `glass_doctor`, which runs off the thread the
        // other tools share — has a file in flight between its dump and its read.
        const THEIRS: &str = "/sdcard/glass_dump_4321_1785700000000000000_7.xml";
        let mut files = HashMap::from([(THEIRS.to_string(), STALE_XML.to_string())]);
        let mut run = fake_device(&mut files);
        settled(dump_once(&mut run, PREFIX, ample())).unwrap();
        drop(run);

        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            vec![THEIRS],
            "removed a file it did not write"
        );
    }

    #[test]
    fn a_read_failure_the_dump_did_not_explain_is_returned_as_it_stands() {
        // The dump succeeded and said so; the read then failed on its own. Blaming the dump
        // here would repeat the misattribution this fix exists to remove.
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "cat", path] => Err(read_err(path)),
                _ => Ok((DUMPED.to_string(), String::new())),
            }
        };
        let e = settled(dump_once(&mut run, PREFIX, ample())).unwrap_err();
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
        dump_until_ready(&mut run, PREFIX, RetryBound::ONCE, Duration::ZERO).unwrap();

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
            PREFIX,
            RetryBound {
                least: 1,
                then_within: Duration::from_secs(5),
            },
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
        // `least: 1`, so the ceiling under test is the only thing stopping the loop.
        let bound = RetryBound {
            least: 1,
            then_within: budget,
        };
        let mut seen: Vec<Instant> = Vec::new();
        let mut cold = fake(usize::MAX);
        let mut run = |argv: &[&str], deadline: Instant| -> Result<(String, String)> {
            seen.push(deadline);
            cold(argv, deadline)
        };
        let started = Instant::now();
        dump_until_ready(&mut run, PREFIX, bound, Duration::from_secs(2))
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
        let e = dump_until_ready(&mut run, PREFIX, patient(), Duration::ZERO).unwrap_err();

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
        let xml = dump_until_ready(&mut run, PREFIX, patient(), Duration::ZERO)
            .expect("a device that becomes ready within the budget must produce a tree");
        assert_eq!(xml, XML);
    }

    #[test]
    fn a_dump_that_never_becomes_ready_fails_with_the_last_reason() {
        let mut run = fake(usize::MAX);
        let e = dump_until_ready(&mut run, PREFIX, RetryBound::ONCE, Duration::ZERO).unwrap_err();
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
        let e = dump_until_ready(&mut run, PREFIX, patient(), Duration::ZERO).unwrap_err();
        assert!(matches!(e, GlassError::Backend(_)), "{e}");
        assert_eq!(
            attempts, 1,
            "must not wait out the budget on a device error"
        );
    }

    #[test]
    fn a_dump_that_died_without_explaining_itself_is_retried() {
        // Measured at 3 failures in 14 runs entering the suite straight from a snapshot restore;
        // a later dump against a settled tree succeeds, so waiting is the whole remedy.
        let mut run = fake_crashing(2);
        let xml = dump_until_ready(&mut run, PREFIX, patient(), Duration::ZERO)
            .expect("a dump that crashed must be retried inside the budget");
        assert_eq!(xml, XML);
    }

    #[test]
    fn a_dump_that_crashed_takes_its_partial_file_with_it() {
        // The crash is raised inside `dumpWindowToFile`, so a dead attempt can still own a file.
        let mut files: HashMap<String, String> = HashMap::new();
        let mut dumps = 0;
        {
            let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
                match argv {
                    ["shell", "uiautomator", "dump", path] => {
                        dumps += 1;
                        // Opened before the walk that dies, so a crashed attempt still has one.
                        files.insert((*path).to_string(), String::new());
                        if dumps > 2 {
                            files.insert((*path).to_string(), XML.to_string());
                            Ok((DUMPED.to_string(), String::new()))
                        } else {
                            Err(crash_err(path))
                        }
                    }
                    ["shell", "cat", path] => match files.get(*path) {
                        Some(xml) => Ok((xml.clone(), String::new())),
                        None => Err(read_err(path)),
                    },
                    ["shell", "rm", "-f", path] => {
                        files.remove(*path);
                        Ok((String::new(), String::new()))
                    }
                    other => panic!("unexpected adb command: {other:?}"),
                }
            };
            dump_until_ready(&mut run, PREFIX, patient(), Duration::ZERO)
                .expect("the attempt after the crashes dumps");
        }

        assert!(
            files.is_empty(),
            "crashed attempts stranded {:?}",
            files.keys().collect::<Vec<_>>()
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

    /// `inner`'s root one level down, under a container that has taken over its id — the shape a
    /// tree that grew a node above the element arrives in.
    fn under_a_container(inner: AxTree) -> AxTree {
        let mut t = tree(AxRole::Group, None, Some(BOUNDS), false);
        t.root.children.push(inner.root);
        t.assign_ids();
        t
    }

    /// A tree with nothing resembling the target in it — Settings' home screen after the search
    /// activity that owned the field was killed (glass#323).
    fn a_different_screen() -> AxTree {
        tree(
            AxRole::Group,
            None,
            Some(AxRect { y: 900, ..BOUNDS }),
            false,
        )
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
        let after = under_a_container(tree_holding(Some("world")));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AxElementChanged(0))
        ));
    }

    #[test]
    fn a_read_back_of_a_screen_that_was_replaced_reports_the_element_gone() {
        // glass#323's other half: the kill can land between the write and the read-back, which
        // is where four of the nine observed refusals came from.
        let t = target(0, Some("Search"), Some(BOUNDS));
        let e = verify_write(&a_different_screen(), &t, "world")
            .expect_err("the field the write was aimed at is not in this tree");
        assert!(matches!(e, GlassError::AxElementGone(0)), "{e}");
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
        // The element is still in the tree, just no longer at that id — renumbered, which is what
        // "changed" means.
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
    fn an_element_that_moved_to_another_id_is_element_changed() {
        // Same id lands on a different element (tree drift) — must refuse, not overwrite, and
        // the element is still there to be re-addressed.
        let t = under_a_container(tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true));
        assert!(matches!(
            locate_editable_target(&t, &target(0, Some("Search"), Some(BOUNDS)), &WIN),
            Err(GlassError::AxElementChanged(0))
        ));
    }

    #[test]
    fn an_element_nothing_in_the_tree_resembles_is_element_gone() {
        // glass#323: a first-boot platform kill destroyed the search activity mid-write, and the
        // id then denoted a container in the tree that replaced it.
        let t = a_different_screen();
        let e = locate_editable_target(&t, &target(0, Some("Search"), Some(BOUNDS)), &WIN)
            .expect_err("a container from another activity is not the field");
        assert!(matches!(e, GlassError::AxElementGone(0)), "{e}");
    }

    #[test]
    fn renaming_a_refusal_never_turns_it_into_a_write() {
        // The taxonomy changes what a refusal is called, not when one happens.
        let field = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true);
        let nested = under_a_container(tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true));
        let replaced = a_different_screen();
        let here = target(0, Some("Search"), Some(BOUNDS));
        let moved = target(0, Some("Search"), Some(AxRect { x: 700, ..BOUNDS }));
        for (what, got) in [
            (
                "a screen that was replaced",
                editable_target(&replaced, &here),
            ),
            (
                "the element one level down",
                editable_target(&nested, &here),
            ),
            ("bounds that drifted", editable_target(&field, &moved)),
            (
                "a name no node carries",
                editable_target(&field, &target(0, Some("Other"), Some(BOUNDS))),
            ),
            (
                "an id that resolves to nothing",
                editable_target(&field, &target(9, Some("Search"), Some(BOUNDS))),
            ),
        ] {
            assert!(got.is_err(), "{what} must not authorise a write");
        }
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
