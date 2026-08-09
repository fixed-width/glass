//! `AndroidA11y` — the Android accessibility reader. Drives `uiautomator dump`
//! over adb and maps the result via `crate::axmap`. Resolves its own device
//! lazily, since the `Accessibility` trait is handed only an `AxContext`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use glass_core::accessibility::{
    Accessibility, AxContext, AxDeadline, AxNode, AxTarget, AxTree, Located,
};
use glass_core::{
    GlassError, KeyEvent, MouseButton, PointerEvent, Result, WindowGeometry, typed_clear_landed,
    typed_text_landed,
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
/// app up — several seconds before the dump can serve one. Later snapshots retry only inside the
/// caller's deadline (see [`snapshot_bound`]); this bound is for the first read, which has no
/// caller's window to borrow.
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
/// it read — must be done by, ignoring any deadline the caller set.
///
/// The three share [`AdbOp::Dump`]'s budget — one snapshot's worth — with the removal keeping its
/// own `AdbOp::Shell` ceiling inside that. A step carrying only its own budget let an attempt cost
/// the sum of all three, 50s against the 10s a `glass_wait_for_element` asks for by default and
/// re-snapshots inside.
///
/// A caller that named a deadline gets [`AxDeadline::cap`] of this instead — see
/// [`dump_until_ready`], which needs both to tell which of them ended an attempt.
pub(crate) fn attempt_deadline() -> Instant {
    Instant::now() + AdbOp::Dump.budget()
}

/// When the removal of a dump file must be done by: `attempt`'s deadline, floored so a spent one
/// still leaves room to remove the file.
///
/// The device writes the file whether or not anyone is still waiting for it and nothing sweeps
/// `/sdcard`, so without the floor every attempt the caller's deadline abandons strands one. It is
/// a ceiling on a millisecond round-trip, not a wait — [`glass_core::TEARDOWN_BUDGET`] is this
/// codebase's bound for cleanup whose own work has already stopped.
fn reap_deadline(attempt: Instant) -> Instant {
    attempt.max(Instant::now() + glass_core::TEARDOWN_BUDGET)
}

/// What a read reports when the caller's deadline, not the device, ended it — naming what the last
/// attempt saw, where there was one.
///
/// [`GlassError::AccessibilityNotReady`] rather than the timeout adb raised: that is the variant
/// `wait_for_element` polls through, keeping a spent budget distinguishable from an element that
/// is genuinely absent (glass#329).
fn out_of_time(last: Option<&GlassError>) -> GlassError {
    let seen = match last {
        Some(e) => format!(" (last attempt: {e})"),
        None => String::new(),
    };
    GlassError::AccessibilityNotReady(format!(
        "uiautomator served no tree within the time this call allowed{seen}"
    ))
}

/// What one dump attempt settled, for a loop deciding whether another would help.
///
/// The judgement is made here, where the failure is diagnosed, rather than inferred by the caller
/// from which error variant escaped: `uiautomator` crashing without a word arrived as an ordinary
/// backend failure, and the `matches!` gate on `AccessibilityUnavailable` that refused to retry it
/// was repaired by restating the error rather than by fixing the gate (glass#341).
pub(crate) enum Attempt {
    Dumped(String),
    /// The device cannot serve a dump *yet* — waiting is what resolves it.
    NotReady(GlassError),
    /// Waiting cannot help *this attempt*: adb is gone, the device is wedged, or a deadline
    /// fired — which may be the caller's, and [`dump_until_ready`] re-reads that case as a spent
    /// budget rather than a device that failed.
    Fatal(GlassError),
}

/// One `uiautomator dump`, returning the XML it wrote. Every step is bounded by `deadline` — see
/// [`dump_until_ready`], which caps [`attempt_deadline`] by the caller's.
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
///
/// It runs on [`reap_deadline`] rather than on `deadline`, which the caller's may have already
/// spent.
pub(crate) fn dump_once(run: &mut AdbRunner<'_>, prefix: &str, deadline: Instant) -> Attempt {
    let path = attempt_path(prefix);
    let stderr = match run(&["shell", "uiautomator", "dump", &path], deadline) {
        Ok((_, stderr)) => stderr,
        Err(e) if died_unexplained(&e) => {
            // The crash is raised after the file is opened, so this attempt can own one. Retried,
            // each crash would strand another.
            let _ = run(&["shell", "rm", "-f", &path], reap_deadline(deadline));
            return Attempt::NotReady(GlassError::AccessibilityUnavailable(format!(
                "uiautomator dump exited without writing {path} and without saying why; \
                 its reason, if any, is in logcat"
            )));
        }
        Err(e) => return Attempt::Fatal(e),
    };
    let read = run(&["shell", "cat", &path], deadline);
    let _ = run(&["shell", "rm", "-f", &path], reap_deadline(deadline));
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

/// Whether an error is a deadline firing rather than the device answering.
///
/// A read that ran out of its time never reached the device, so the dump's stderr is no
/// explanation for it — and that substitution is retryable, so the loop would go on retrying a
/// device that had answered.
///
/// Says only *that* a bound fired, not which deadline governed — [`dump_until_ready`] settles
/// that before the attempt. Read off the bound's signal rather than the message, which
/// `Adb::exit_error` composes partly from the device's output (glass#348).
fn bound_fired(e: &GlassError) -> bool {
    e.bound().is_some()
}

/// Whether a failed dump gave no reason of its own — the mark of a `uiautomator` that crashed.
///
/// It dies with a `NullPointerException` walking a tree that is still changing, exiting non-zero
/// with an empty stderr because the trace goes to logcat via `AndroidRuntime` (glass#341). That
/// resolves by waiting. adb's own failures — a device that is gone, a wedged server — always carry
/// a reason and do not.
///
/// Read off the stderr the error carries rather than the message it renders, which ends in that
/// same stderr: a device whose last word was "failed:" used to answer this for `uiautomator`
/// (glass#348).
fn died_unexplained(e: &GlassError) -> bool {
    e.tool_said().is_some_and(str::is_empty)
}

/// How long a readiness wait may retry for, and how many attempts it owes regardless.
///
/// A wall-clock budget alone cannot express "try twice": one attempt may cost up to
/// [`AdbOp::Dump`]'s whole budget, so a budget shorter than two of those can expire before a second
/// attempt ever starts. That is what a 2s budget did against attempts measured 3.5s apart — a retry
/// budget that never retried (glass#338).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetryBound {
    /// Attempts owed however long each one takes, while the caller is still waiting; 1 means no
    /// retry at all.
    least: u32,
    /// Once `least` is met, keep retrying while this much wall-clock remains, or the caller's
    /// deadline, whichever is nearer.
    then_within: Duration,
}

impl RetryBound {
    /// Exactly one attempt.
    const ONCE: Self = Self {
        least: 1,
        then_within: Duration::ZERO,
    };
}

/// Dump, retrying while `uiautomator` cannot serve one yet, within `bound` and `caller`.
///
/// Only [`Attempt::NotReady`] is retried, so a device that has gone away is reported at once
/// rather than waited on.
///
/// Returns after `bound.least` attempts plus however many more start while `bound.then_within`
/// remains — each costing up to `AdbOp::Dump`'s budget, per [`attempt_deadline`] — and no later
/// than `caller`, which caps both the attempts and the window they retry in.
fn dump_until_ready(
    run: &mut AdbRunner<'_>,
    prefix: &str,
    bound: RetryBound,
    interval: Duration,
    caller: AxDeadline,
) -> Result<String> {
    let retry_until = caller.cap(Instant::now() + bound.then_within);
    let mut owed = bound.least.max(1);
    // Kept so a spent caller deadline can name what the device last said, not just the budget.
    let mut unready: Option<GlassError> = None;
    loop {
        // Answered here rather than through the cap below: the runner seam does not promise a
        // spent deadline is refused, and a read that never happened should not have to be
        // inferred from the error it returns.
        if caller.has_passed() {
            return Err(out_of_time(unready.as_ref()));
        }
        let own = attempt_deadline();
        let ends = caller.cap(own);
        match dump_once(run, prefix, ends) {
            Attempt::Dumped(xml) => return Ok(xml),
            // An attempt the *caller's* deadline cut short is not a device that failed. Which
            // bound governs is settled before the attempt, not read off its error (glass#341).
            //
            // The abandoned attempt's own error is carried, not dropped: a wedged adb reaches
            // here too — it cannot be told apart from a slow one at the moment the budget ends —
            // and its message is the only place glass names the `adb kill-server` remedy.
            Attempt::Fatal(e) if caller.governs(own) && bound_fired(&e) => {
                return Err(out_of_time(Some(&e)));
            }
            Attempt::Fatal(e) => return Err(e),
            Attempt::NotReady(e) => {
                owed = owed.saturating_sub(1);
                let left = retry_until.saturating_duration_since(Instant::now());
                if owed == 0 && left.is_zero() {
                    // The caller closing the window is about the budget, not the device — even
                    // with the device's own answer in hand.
                    return Err(if caller.has_passed() {
                        out_of_time(Some(&e))
                    } else {
                        e
                    });
                }
                unready = Some(e);
                // Clamped, so an attempt the ceiling still governs starts inside it — unclamped, a
                // whole further attempt would land past the bound. An owed attempt waits only what
                // is left, which a spent ceiling makes zero.
                std::thread::sleep(interval.min(left));
            }
        }
    }
}

/// Judge a typed `set_value` from a tree read back after it. `Ok(())` only when the field holds
/// exactly what was asked for, or — for a named field — reads back empty after a clear.
///
/// Exact match, not "changed from before": tap-and-type is not atomic, so a dropped key or an input
/// filter leaves the field holding something that is neither the request nor the old value, and
/// calling that success is the failure this check exists to prevent. `glass_core::typed_text_landed`
/// and `glass_core::typed_clear_landed` carry the rules and the cost of them. Twin of `verify_write`
/// in `glass-ios/src/a11y.rs` — keep the two in step.
///
/// The element is re-resolved by identity (role + name) via [`AxTarget::relocate`], not by its
/// pre-order id. The soft keyboard does not itself shift ids — measured on the dogfood AVD with
/// `mInputShown=true`, `uiautomator dump` emits the focused window only and no IME window — but a
/// tap that navigates does: Settings' search entry opens a different window, and the field can turn
/// up renumbered inside it rather than gone.
///
/// Bounds are deliberately no part of that identity, unlike the pre-write
/// [`locate_editable_target`]: the IME reflows the layout under the field it is typing into, so a
/// moved-but-correct element is the normal case here. `relocate` does require a re-found node to
/// still *overlap* where the target was, which is what separates that reflow from a field on
/// another screen entirely — and this reader needs it most, because [`crate::axmap::labels`] names
/// an editable from its resource-id leaf whenever `content-desc` is absent, which is most of the
/// time. A leaf like `search_src_text` names a *layout*: it repeats across every instance of it,
/// and across apps, so a navigating tap can find the next screen's field matching uniquely.
///
/// Both arms carry the target's role and name, so a node reached here that is not editable is not a
/// neighbour that inherited the id; it is the identified element having lost editability. The write
/// has already dispatched by this point, so that is [`GlassError::AxWriteUnconfirmed`], not the
/// [`AxTarget::drift_error`].
///
/// A clear (`text` empty) is confirmed only for a target that carries a name. An empty read-back is
/// what any untouched same-role, same-name field would also show, so it is evidence of the write
/// only inasmuch as the identity that found the field is discriminating — a non-empty write's
/// exact-text check grounds itself, a clear has nothing to. A nameless target is refused however it
/// was reached, its own id included: [`AxTarget::matches`] compares `None` to `None`, so `AtId`
/// there accepts whatever nameless editable renumbering slid onto the id.
fn verify_write(after_tree: &AxTree, target: &AxTarget, text: &str) -> Result<()> {
    let node = match target.relocate(after_tree) {
        Located::AtId(node) | Located::Moved(node) => node,
        Located::Ambiguous(candidates) => {
            return Err(GlassError::AxWriteUnconfirmed(
                target.id.0,
                format!(
                    "{} elements now match its role and name, so which one holds it cannot be told",
                    candidates.len()
                ),
            ));
        }
        Located::Gone => {
            return Err(GlassError::AxWriteUnconfirmed(
                target.id.0,
                "nothing in the tree carries its role and name, so the screen it was on was \
                 replaced"
                    .into(),
            ));
        }
        Located::Unproven => {
            // Keep the cap in the message: it is what tells a caller to raise `max_nodes`. A
            // complete tree has no hole to blame, so the one way it
            // reaches here is a lone match drawn clear of where the element was.
            let why = if let Some(t) = &after_tree.truncated {
                format!(
                    "the tree was truncated at {} {}, so the element — or a second one matching \
                     it — may be past the cap",
                    t.limit_value,
                    t.limit.label()
                )
            } else if after_tree.unreadable > 0 {
                "a subtree could not be read, so the element — or a second one matching it — may \
                 be inside it"
                    .to_string()
            } else {
                "the only element carrying its role and name is drawn clear of where this one \
                 was, so it may be a different one"
                    .to_string()
            };
            return Err(GlassError::AxWriteUnconfirmed(target.id.0, why));
        }
    };
    if !target.bounds_overlap(node.bounds) {
        return Err(GlassError::AxWriteUnconfirmed(
            target.id.0,
            "the element carrying its role and name is drawn clear of where this one was, so the \
             text may have gone to a different one"
                .into(),
        ));
    }
    if !node.states.editable {
        return Err(GlassError::AxWriteUnconfirmed(
            target.id.0,
            "the element at that id is no longer editable, so the value could not be read back"
                .into(),
        ));
    }
    if text.is_empty() && target.name.is_none() {
        return Err(GlassError::AxWriteUnconfirmed(
            target.id.0,
            "the element has no name, so nothing but its position identifies it and an empty \
             field cannot confirm a clear landed on it"
                .into(),
        ));
    }
    let landed = if text.is_empty() {
        typed_clear_landed(node.value.as_deref())
    } else {
        typed_text_landed(node.value.as_deref(), text)
    };
    if landed {
        Ok(())
    } else {
        // `Some("")`, not `None`: this mapper drops an empty value, so an empty field arrives here
        // as `None` — the same `None` the verdict reserves for a reading nobody obtained.
        Err(GlassError::value_not_applied(
            target.id.0,
            text,
            Some(node.value.as_deref().unwrap_or("")),
        ))
    }
}

/// The read-back's own failure, reported as an unconfirmed write.
///
/// Post-dispatch by construction: its only caller is the loop that runs after the keystrokes went
/// out. A verdict `GlassError::set_value_failed_after_writing` rejects would leave the session
/// holding the value it cached for a write that already landed, and refuse the retry as drift.
fn read_back_failed(target: &AxTarget, e: &GlassError) -> GlassError {
    GlassError::AxWriteUnconfirmed(target.id.0, format!("reading the element back failed: {e}"))
}

/// How many times to read the element back before reporting the write as not applied. A landed
/// write confirms on the first read and pays for one; the retries exist for a field that commits a
/// frame or two later — a Compose recompose, a debounced handler — which the on-device service
/// reader polls two whole seconds for.
const VERIFY_ATTEMPTS: usize = 3;
const _: () = assert!(
    VERIFY_ATTEMPTS > 0,
    "set_value reports the last read-back, so there must be one"
);

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

/// One snapshot over a supplied runner: the whole of [`AndroidA11y::snapshot_within`] except
/// binding to a device.
///
/// Split out so a host test can see the join: inline, replacing `ctx.deadline` with
/// [`AxDeadline::UNBOUNDED`] left every non-device test green.
fn snapshot_with_runner(
    run: &mut AdbRunner<'_>,
    ctx: &AxContext,
    bound: RetryBound,
) -> Result<AxTree> {
    let xml = dump_until_ready(
        run,
        DUMP_PREFIX,
        bound,
        Duration::from_millis(DUMP_POLL_INTERVAL_MS),
        ctx.deadline,
    )?;
    build_tree(&xml, &ctx.window, ctx.limits)
}

/// Whether a dump has ever succeeded on this reader, which decides how long its next one may wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Warmth {
    /// No dump has succeeded yet, so the accessibility bridge may still be registering.
    Cold,
    /// One has, so the device can serve a tree and a read that cannot is a passing condition.
    Warmed,
}

/// How much retrying one [`Accessibility::snapshot`] is worth, from whether a dump has ever
/// succeeded on this reader and how long the caller said it would wait.
///
/// A warmed reader gets no retry of its own — only what the caller's deadline pays for. Retrying
/// without one is what a `glass_wait_for_element` cannot afford: it re-reads on its own schedule,
/// so a second attempt here costs it another whole [`AdbOp::Dump`] budget past its timeout
/// (glass#338).
///
/// The cold wait is not capped here: [`dump_until_ready`] caps every wait by the caller, and this
/// one must still *owe* the two attempts the accessibility bridge's registration needs.
fn snapshot_bound(warmth: Warmth, deadline: AxDeadline) -> RetryBound {
    if warmth == Warmth::Cold {
        return COLD_BOUND;
    }
    match deadline.remaining() {
        // Nothing bounds a retry, so there is no retry.
        None => RetryBound::ONCE,
        Some(left) => RetryBound {
            least: 1,
            then_within: left,
        },
    }
}

/// Reads the active window's accessibility tree via `uiautomator`.
pub struct AndroidA11y {
    adb: Adb,
    resolved: bool,
    /// Set once a dump has succeeded, after which snapshots stop waiting for readiness.
    warmed: bool,
    /// `GLASS_ANDROID_SERIAL` as it stood when `new` was called — `for_adb` is already resolved
    /// and leaves it unset. Read once rather than at resolution, so the device a session is
    /// reading cannot change under it midway.
    want_serial: Option<String>,
}

impl AndroidA11y {
    pub fn new() -> Self {
        Self {
            adb: Adb::from_env(),
            resolved: false,
            warmed: false,
            want_serial: std::env::var("GLASS_ANDROID_SERIAL").ok(),
        }
    }

    /// One dump, retrying a not-ready device within `bound` and `ctx.deadline`.
    ///
    /// Split out of [`Accessibility::snapshot`] so a caller that knows the UI is mid-flux can ask
    /// for retries a [`snapshot_bound`] would not give it — immediately after typing, where the
    /// tree is expected to be unreadable for a moment and no deadline says how long to allow.
    fn snapshot_within(&mut self, ctx: &AxContext, bound: RetryBound) -> Result<AxTree> {
        let adb = self.ensure_adb()?;
        let tree = snapshot_with_runner(&mut adb_runner(&adb), ctx, bound)?;
        self.warmed = true;
        Ok(tree)
    }

    /// Bind directly to an already-resolved (serial-bound) adb client. Used in production so
    /// the reader talks to the exact device the platform resolved, instead of re-resolving.
    pub fn for_adb(adb: Adb) -> Self {
        Self {
            adb,
            resolved: true,
            warmed: false,
            want_serial: None,
        }
    }

    /// Whether a dump has ever succeeded on this reader.
    fn warmth(&self) -> Warmth {
        if self.warmed {
            Warmth::Warmed
        } else {
            Warmth::Cold
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
            let serial = choose_serial(self.want_serial.as_deref(), &online)?;
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
///
/// An id that resolves to nothing stays [`GlassError::AxElementNotFound`];
/// [`AxTarget::drift_error`] classifies only an id occupied by something unrelated.
pub(crate) fn fingerprinted<'a>(tree: &'a AxTree, target: &AxTarget) -> Result<&'a AxNode> {
    let node = tree
        .find(target.id)
        .ok_or(GlassError::AxElementNotFound(target.id.0))?;
    if !target.matches(node.role, node.name.as_deref())
        || !target.bounds_consistent(node.bounds, 8)
        || !target.value_consistent(node.value.as_deref())
    {
        return Err(target.drift_error(tree));
    }
    Ok(node)
}

/// Re-resolve `target` in an already-numbered `tree` and return the node only if it is still the
/// element that was addressed and still editable. Errors specifically when the id resolves to
/// nothing (`AxElementNotFound`), when it has drifted in role/name/bounds/value
/// (`AxElementChanged`, or `AxElementGone` where nothing in the tree presents as it — see
/// [`AxTarget::drift_error`]), or when it is not editable (`AxElementNotEditable`).
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

/// Readiness bound for the read a write makes before it can act.
///
/// `uiautomator` serves no tree for a moment while a window transition finishes or the
/// accessibility bridge re-registers, and one attempt turns that into `AccessibilityUnavailable`
/// where the caller asked about an element (glass#338).
///
/// Same shape as [`VERIFY_BOUND`] and kept separate from it: they wait out different moments, so a
/// change to one is not a change to the other. The cost is that a write against a device that never
/// serves a tree now pays two [`AdbOp::Dump`] budgets here rather than one, and `set_value` carries
/// no caller deadline to cap that — do not add a third attempt without one.
const PRE_WRITE_BOUND: RetryBound = RetryBound {
    least: 2,
    then_within: Duration::ZERO,
};

/// Read the tree and locate `target`'s tap point — the read every write makes before it can act.
///
/// Over a runner so the bound above is testable without a device — the rest of a write taps and
/// types through `Adb` directly.
fn locate_for_write(
    run: &mut AdbRunner<'_>,
    ctx: &AxContext,
    target: &AxTarget,
) -> Result<(i32, i32)> {
    let mut tree = snapshot_with_runner(run, ctx, PRE_WRITE_BOUND)?;
    tree.assign_ids();
    locate_editable_target(&tree, target, &ctx.window)
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
        self.snapshot_within(ctx, snapshot_bound(self.warmth(), ctx.deadline))
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let window = ctx.window.clone();
        let adb = self.ensure_adb()?;
        // Re-snapshot and number nodes to locate the target by its pre-order id.
        let (cx, cy) = locate_for_write(&mut adb_runner(&adb), ctx, target)?;
        self.warmed = true;
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
            let mut after = self
                .snapshot_within(ctx, VERIFY_BOUND)
                .map_err(|e| read_back_failed(target, &e))?;
            after.assign_ids();
            match verify_write(&after, target, text) {
                Ok(()) => return Ok(()),
                // Only a not-applied verdict can change on a later read: drift and truncation are
                // structural, and re-dumping for them costs seconds to reach the same answer.
                Err(e @ GlassError::AxValueNotApplied { .. }) => last = Some(e),
                Err(e) => return Err(e),
            }
            if Instant::now() >= phase_ends {
                break;
            }
        }
        // The const assert on `VERIFY_ATTEMPTS` is what makes `last` always set; the fallback only
        // avoids an unwrap, and names no read-back because none was taken.
        Err(last.unwrap_or_else(|| GlassError::value_not_applied(target.id.0, text, None)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Attempt, COLD_BOUND, RetryBound, Warmth, bound_fired, dump_once, dump_until_ready,
        editable_target, locate_editable_target, locate_for_write, read_back_failed,
        snapshot_bound, snapshot_with_runner, verify_write,
    };
    use crate::adb::{AdbOp, a_failed_call, a_real_spawn_failure, a_real_timeout_hinted};
    use glass_core::accessibility::{
        AxContext, AxDeadline, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree,
        Located, WalkLimits,
    };
    use glass_core::{BoundKind, GlassError, Result, WindowGeometry};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// A deadline no test reaches, for the cases that are not about the bound.
    fn ample() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// A spent-deadline error as `glass_core` really raises one. Nothing is spawned on that path,
    /// so it needs no real command.
    fn never_started_for_want_of_time() -> GlassError {
        glass_core::run_bounded_until(
            &mut std::process::Command::new("adb"),
            Duration::from_secs(10),
            Instant::now(),
            "adb:uiautomator dump",
        )
        .expect_err("a spent deadline starts nothing")
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
        a_failed_call(
            &["shell", "cat", path],
            &format!("cat: {path}: No such file"),
        )
    }

    /// What `Adb` raises for the crash [`died_unexplained`] names: non-zero exit, empty stderr.
    ///
    /// Built by `Adb`'s own constructor, so a fixture cannot go on describing a crash the classifier
    /// has stopped recognising.
    fn crash_err(path: &str) -> GlassError {
        a_failed_call(&["shell", "uiautomator", "dump", path], "")
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

    /// An adb whose `uiautomator dump` fails with `said` on stderr, answering every other call —
    /// the `rm -f` that follows a crash included — emptily.
    ///
    /// `said` is the whole variable: empty is the crash [`died_unexplained`] names, anything else a
    /// device that gave a reason.
    fn fake_failing_dump(
        said: &'static str,
    ) -> impl FnMut(&[&str], Instant) -> Result<(String, String)> {
        move |argv: &[&str], _deadline: Instant| match argv {
            ["shell", "uiautomator", "dump", _] => Err(a_failed_call(argv, said)),
            _ => Ok((String::new(), String::new())),
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
            AxDeadline::UNBOUNDED,
        )
        .expect("the second attempt is owed however long the first took");

        assert_eq!(xml, XML);
        assert_eq!(dumps, 2, "exactly the attempt that was owed");
    }

    /// A runner that answers every step, recording the subcommand and the deadline each was given.
    fn recording(
        seen: &mut Vec<(String, Instant)>,
    ) -> impl FnMut(&[&str], Instant) -> Result<(String, String)> {
        move |argv: &[&str], deadline: Instant| {
            seen.push((
                argv.get(1).copied().unwrap_or_default().to_string(),
                deadline,
            ));
            match argv {
                ["shell", "uiautomator", "dump", _] => Ok((DUMPED.to_string(), String::new())),
                ["shell", "cat", _] => Ok((XML.to_string(), String::new())),
                ["shell", "rm", "-f", _] => Ok((String::new(), String::new())),
                other => panic!("unexpected adb command: {other:?}"),
            }
        }
    }

    /// glass#338: `wait_for_element` re-reads from a synchronous tick, so only the reader can
    /// hold a read inside the timeout.
    ///
    /// The removal is excluded deliberately — it is not part of the answer the caller is waiting
    /// for, and [`reap_deadline`] floors it so a spent caller cannot skip it. It is asserted below
    /// instead, against that floor, so it still cannot hang.
    #[test]
    fn every_read_step_is_bounded_by_the_callers_deadline_not_by_the_dump_budget() {
        let mut seen: Vec<(String, Instant)> = Vec::new();
        {
            let mut run = recording(&mut seen);
            dump_until_ready(
                &mut run,
                PREFIX,
                RetryBound::ONCE,
                Duration::ZERO,
                AxDeadline::from_millis(50),
            )
            .expect("the dump succeeds");
        }

        // The caller's instant was fixed before this line, so it passes however slow the machine
        // is; `AdbOp::Dump`'s 20s budget cannot.
        let latest = Instant::now() + Duration::from_millis(50);
        let reads: Vec<_> = seen.iter().filter(|(step, _)| step != "rm").collect();
        assert_eq!(reads.len(), 2, "expected the dump and the read: {seen:?}");
        for (step, d) in reads {
            assert!(
                *d <= latest,
                "the {step} step was given {:?} past the caller's deadline",
                d.saturating_duration_since(latest)
            );
        }

        let reap = seen
            .iter()
            .find(|(step, _)| step == "rm")
            .expect("the attempt removes the file it read");
        assert!(
            reap.1 <= Instant::now() + glass_core::TEARDOWN_BUDGET,
            "the removal outlives its own floor, so a wedged device can hang on cleanup"
        );
    }

    /// A file the caller stopped waiting for is still removed. The device writes it whether or not
    /// anyone reads it, and nothing sweeps `/sdcard` — so a removal that inherited the spent
    /// deadline would strand one file per abandoned attempt, which every timed-out wait produces.
    #[test]
    fn the_removal_survives_an_attempt_deadline_that_is_already_spent() {
        let spent = Instant::now();
        let mut seen: Vec<(String, Instant)> = Vec::new();
        {
            let mut run = recording(&mut seen);
            let _ = dump_once(&mut run, PREFIX, spent);
        }

        let reap = seen
            .iter()
            .find(|(step, _)| step == "rm")
            .expect("the attempt removes the file it read");
        assert!(
            reap.1 > spent,
            "the removal inherited a deadline it could not run in"
        );
    }

    /// Without this the test above passes on a reader that caps every step at nothing.
    #[test]
    fn a_caller_that_names_no_deadline_leaves_the_reader_its_own_budget() {
        let mut seen: Vec<(String, Instant)> = Vec::new();
        let at_least = Instant::now() + AdbOp::Dump.budget() - Duration::from_secs(1);
        {
            let mut run = recording(&mut seen);
            dump_until_ready(
                &mut run,
                PREFIX,
                RetryBound::ONCE,
                Duration::ZERO,
                AxDeadline::UNBOUNDED,
            )
            .expect("the dump succeeds");
        }

        assert!(!seen.is_empty(), "no adb step ran");
        assert!(
            seen.iter().all(|(_, d)| *d >= at_least),
            "a caller that asked for nothing had the dump budget cut anyway"
        );
    }

    /// The error names the budget rather than a device that was never consulted.
    #[test]
    fn a_dump_the_caller_stopped_waiting_for_is_not_started() {
        let mut ran = false;
        let mut run = |_argv: &[&str], _d: Instant| -> Result<(String, String)> {
            ran = true;
            Ok((DUMPED.to_string(), String::new()))
        };
        let e = dump_until_ready(
            &mut run,
            PREFIX,
            patient(),
            Duration::ZERO,
            AxDeadline::from_millis(0),
        )
        .expect_err("the caller had stopped waiting");

        assert!(
            !ran,
            "an adb step ran for a caller that had stopped waiting"
        );
        assert!(matches!(e, GlassError::AccessibilityNotReady(_)), "{e}");
    }

    /// glass#338: before the deadline reached the reader, one attempt was the only safe number —
    /// nothing else bounded a retry.
    #[test]
    fn a_warmed_reader_retries_only_inside_the_deadline_the_caller_named() {
        let told = snapshot_bound(Warmth::Warmed, AxDeadline::from_millis(5_000));
        assert!(
            told.then_within > Duration::from_secs(4) && told.then_within <= Duration::from_secs(5),
            "the retry window is not the caller's: {told:?}"
        );

        let untold = snapshot_bound(Warmth::Warmed, AxDeadline::UNBOUNDED);
        assert_eq!(
            untold.then_within,
            Duration::ZERO,
            "retried on a budget no caller had agreed to: {untold:?}"
        );
        assert_eq!(untold.least, 1, "{untold:?}");
    }

    /// [`snapshot_bound`] does not pre-trim the cold bound to what the caller has left —
    /// `dump_until_ready` is the single place the caller's deadline is applied, so trimming here
    /// would apply it twice and leave the attempts the bridge's registration needs unasked.
    #[test]
    fn a_cold_reader_is_owed_its_attempts_however_little_the_caller_allowed() {
        let bound = snapshot_bound(Warmth::Cold, AxDeadline::from_millis(1));
        assert_eq!(bound.least, COLD_BOUND.least, "{bound:?}");
        assert_eq!(bound.then_within, COLD_BOUND.then_within, "{bound:?}");
    }

    /// Uncapped, a [`RetryBound`] outliving the caller would sleep out a whole interval before
    /// noticing nobody is waiting — and the interval is sized against a cold boot, not against any
    /// one caller.
    #[test]
    fn the_retry_window_ends_with_the_caller_even_when_the_bound_would_run_longer() {
        let mut run = |argv: &[&str], _d: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", _] => {
                    // Outlasts the caller's deadline, so it expires mid-attempt rather than before
                    // the loop can start one.
                    std::thread::sleep(Duration::from_millis(30));
                    Ok((String::new(), format!("{NOT_READY}\n")))
                }
                ["shell", "cat", path] => Err(read_err(path)),
                ["shell", "rm", "-f", _] => Ok((String::new(), String::new())),
                other => panic!("unexpected adb command: {other:?}"),
            }
        };
        let started = Instant::now();
        let e = dump_until_ready(
            &mut run,
            PREFIX,
            patient(),
            Duration::from_secs(5),
            AxDeadline::from_millis(10),
        )
        .expect_err("the caller's deadline passed during the attempt");

        assert!(matches!(e, GlassError::AccessibilityNotReady(_)), "{e}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "slept a retry interval for a caller that had stopped waiting: {:?}",
            started.elapsed()
        );
    }

    /// The caller's deadline in the `AxContext` is what bounds the adb steps — the join between
    /// the seam and the reader, which only the device test covered.
    #[test]
    fn a_snapshot_bounds_its_steps_by_the_deadline_in_the_context() {
        let mut seen: Vec<(String, Instant)> = Vec::new();
        let ctx = AxContext {
            pids: vec![],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: AxDeadline::from_millis(50),
        };
        {
            let mut run = recording(&mut seen);
            snapshot_with_runner(&mut run, &ctx, RetryBound::ONCE).expect("the dump succeeds");
        }

        let latest = Instant::now() + Duration::from_millis(50);
        let reads: Vec<_> = seen.iter().filter(|(step, _)| step != "rm").collect();
        assert_eq!(reads.len(), 2, "expected the dump and the read: {seen:?}");
        for (step, d) in reads {
            assert!(
                *d <= latest,
                "the {step} step ignored the context's deadline by {:?}",
                d.saturating_duration_since(latest)
            );
        }
    }

    /// glass#338 row 1: the read a write makes before it can act is owed a second attempt.
    ///
    /// The recorded failure is a `set_value` on a deliberately stale target answering
    /// `AccessibilityUnavailable("…null root node…")` instead of one of the three element verdicts
    /// the caller asked for.
    ///
    /// The empty tree is what discriminates: a read that got through reports the element missing, a
    /// read that did not reports readiness.
    #[test]
    fn the_read_before_a_write_is_owed_a_second_attempt() {
        let mut cold = fake(1);
        let target = AxTarget {
            id: AxNodeId(7),
            role: AxRole::TextField,
            name: Some("Search".into()),
            bounds: None,
            value: None,
        };

        let e = locate_for_write(&mut cold, &write_ctx(), &target)
            .expect_err("the fixture tree holds no element to write into");

        assert!(
            matches!(e, GlassError::AxElementNotFound(_)),
            "the write's own read gave up on a device that was ready a moment later: {e}"
        );
    }

    /// The context a write reads under: no caller deadline, which is what `set_value` is handed.
    fn write_ctx() -> AxContext {
        AxContext {
            pids: vec![],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: AxDeadline::UNBOUNDED,
        }
    }

    /// The retry the deadline exists to permit actually happens: a caller still waiting gets its
    /// second attempt.
    ///
    /// Both other `owed` tests use a caller who named nothing or one already gone, so an
    /// implementation reading any deadline at all as "no retries" passes them — and would give a
    /// cold Android reader one attempt under every `wait_for_element`, which is the bridge-
    /// registration race of glass#338 back again.
    #[test]
    fn a_retry_the_caller_still_has_time_for_is_taken() {
        let mut cold = fake(1);
        let xml = dump_until_ready(
            &mut cold,
            PREFIX,
            COLD_BOUND,
            Duration::ZERO,
            AxDeadline::from_millis(5_000),
        )
        .expect("the second attempt runs inside the window the caller is still waiting through");

        assert_eq!(xml, XML);
    }

    /// A device that failed is reported as one even while a deadline is in force, which is the
    /// whole load `bound_fired` carries in that guard.
    ///
    /// Without this, dropping `bound_fired` kills no test: every other test reaching the guard
    /// either has no caller deadline, so the guard is false whatever the error, or pairs a
    /// deadline with a timeout, so both halves are true. A `wait_for_element` always sets a
    /// deadline, so the mutant would report every dead emulator as an app that is slow to publish.
    #[test]
    fn an_adb_failure_under_a_live_deadline_is_still_a_broken_device() {
        let mut gone = fake_failing_dump("error: device 'emulator-5554' not found");
        let e = dump_until_ready(
            &mut gone,
            PREFIX,
            RetryBound::ONCE,
            Duration::ZERO,
            // Nearer than `AdbOp::Dump`'s budget, so the caller's bound governs the attempt —
            // which is true of every read a `wait_for_element` makes.
            AxDeadline::from_millis(5_000),
        )
        .expect_err("the device is gone");

        assert_eq!(
            e.tool_said(),
            Some("error: device 'emulator-5554' not found"),
            "a dead device's own reason was replaced: {e}"
        );
        assert!(
            !e.to_string().contains("within the time this call allowed"),
            "a dead device was reported as a spent caller budget: {e}"
        );
    }

    /// Both bounds `glass_core` fires are the ones the classifier reads, checked against errors it
    /// really raised rather than against a constant both sides share.
    ///
    /// `bound_fired` recognises any [`BoundKind`], so what it can miss is a bound arriving as some
    /// other variant — which a wrapper on the way out of `Adb` could silently do, and no test of
    /// either side alone would see.
    #[test]
    fn every_bound_glass_core_fires_is_one_the_classifier_recognises() {
        for (want, e) in [
            (BoundKind::TimedOut, a_real_timeout_hinted()),
            (BoundKind::NotStarted, never_started_for_want_of_time()),
        ] {
            assert_eq!(e.bound(), Some(want), "{e}");
            assert!(bound_fired(&e), "{e}");
        }
    }

    /// A crash that managed only a newline is still a crash — `exit_error` trims on the way in and
    /// [`GlassError::tool_said`] on the way out.
    #[test]
    fn a_dump_that_crashed_writing_only_whitespace_is_still_retried() {
        let mut barely = fake_failing_dump("\n  ");
        assert!(matches!(
            dump_once(&mut barely, PREFIX, ample()),
            Attempt::NotReady(_)
        ));
    }

    /// A call that never reached the tool is not a tool that said nothing.
    ///
    /// Without this nothing exercises `Backend` through `dump_once` at all: every other fake in
    /// this module raises the variant a tool that *ran* produces.
    #[test]
    fn a_dump_whose_adb_could_not_start_is_not_the_crash_that_waiting_resolves() {
        let mut missing = |_argv: &[&str], _d: Instant| -> Result<(String, String)> {
            Err(a_real_spawn_failure())
        };
        assert!(matches!(
            dump_once(&mut missing, PREFIX, ample()),
            Attempt::Fatal(_)
        ));
    }

    /// Only the *dump* step's silence is retried. A `cat` that dies the same way is fatal — the
    /// dump already reported success, so a read that cannot speak for itself is about the device.
    #[test]
    fn a_read_that_failed_silently_is_not_retried_the_way_a_silent_dump_is() {
        let mut run = |argv: &[&str], _d: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "cat", path] => Err(a_failed_call(&["shell", "cat", path], "")),
                _ => Ok((DUMPED.to_string(), String::new())),
            }
        };
        assert!(matches!(
            dump_once(&mut run, PREFIX, ample()),
            Attempt::Fatal(_)
        ));
    }

    /// Text the device wrote is not a bound of glass's own. `Adb::exit_error` interpolates the
    /// child's stderr verbatim, so keyed on the message this was reported as a spent budget —
    /// which `wait_for_element` polls through rather than surfaces (glass#348).
    ///
    /// The stderr below is constructed rather than observed: no device is known to print it, and
    /// nothing stops one.
    #[test]
    fn a_device_failure_that_quotes_the_deadline_wording_is_still_a_broken_device() {
        let mut quoting =
            fake_failing_dump("java.lang.IllegalStateException: UiAutomation was not started");
        let e = dump_until_ready(
            &mut quoting,
            PREFIX,
            RetryBound::ONCE,
            Duration::ZERO,
            AxDeadline::from_millis(5_000),
        )
        .expect_err("the device failed");

        assert_eq!(e.bound(), None, "{e}");
        assert!(
            e.to_string().contains("IllegalStateException"),
            "the device's own reason was replaced: {e}"
        );
    }

    /// [`RetryBound::least`] says how many attempts the *device* is owed, not how many a caller
    /// must pay for: an owed attempt is not started once the caller has stopped waiting. Both
    /// *retrying* bounds owe two, so this is the shape a real not-ready read takes.
    #[test]
    fn an_owed_attempt_is_not_started_once_the_caller_has_stopped_waiting() {
        let mut dumps = 0;
        let mut run = |argv: &[&str], _d: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", _] => {
                    dumps += 1;
                    // Outlasts the caller, so the deadline passes with an attempt still owed.
                    std::thread::sleep(Duration::from_millis(30));
                    Ok((String::new(), format!("{NOT_READY}\n")))
                }
                ["shell", "cat", path] => Err(read_err(path)),
                ["shell", "rm", "-f", _] => Ok((String::new(), String::new())),
                other => panic!("unexpected adb command: {other:?}"),
            }
        };
        let e = dump_until_ready(
            &mut run,
            PREFIX,
            COLD_BOUND,
            Duration::ZERO,
            AxDeadline::from_millis(10),
        )
        .expect_err("the caller's deadline passed during the first attempt");

        assert_eq!(dumps, 1, "the owed attempt ran for a caller that had gone");
        assert!(
            e.to_string().contains("null root node"),
            "the spent-budget error dropped what the device had said: {e}"
        );
    }

    /// An attempt the caller's deadline cut short is a spent budget, not a broken device — and a
    /// wait told the difference reports `{matched:false}` instead of failing.
    #[test]
    fn an_attempt_the_caller_cut_short_reports_the_budget_rather_than_the_device() {
        let timed_out = |_argv: &[&str], _d: Instant| -> Result<(String, String)> {
            Err(a_real_timeout_hinted())
        };

        let mut cut_short = timed_out;
        let e = dump_until_ready(
            &mut cut_short,
            PREFIX,
            RetryBound::ONCE,
            Duration::ZERO,
            AxDeadline::from_millis(20),
        )
        .expect_err("the attempt was abandoned");
        assert!(matches!(e, GlassError::AccessibilityNotReady(_)), "{e}");
        // A wedged adb reaches this arm too, and its message is the only place glass names the
        // `adb kill-server` remedy — dropping it leaves an operator reading "the app is slow".
        assert!(
            e.to_string().contains("adb kill-server"),
            "the abandoned attempt's own error was discarded: {e}"
        );

        // The control: the same timeout with no caller deadline is glass's own 20s budget firing,
        // which *is* about the device — reporting it as a spent caller budget would hide a hang.
        let mut hung = timed_out;
        let e = dump_until_ready(
            &mut hung,
            PREFIX,
            RetryBound::ONCE,
            Duration::ZERO,
            AxDeadline::UNBOUNDED,
        )
        .expect_err("the attempt timed out");
        assert_eq!(e.bound(), Some(BoundKind::TimedOut), "{e}");
    }

    /// Retryability is the attempt's own verdict, not a guess from the error variant it carries:
    /// both of these are `GlassError::ToolFailed` underneath, differing only in what the tool said.
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

        let mut gone = fake_failing_dump("device offline");
        assert!(matches!(
            dump_once(&mut gone, PREFIX, ample()),
            Attempt::Fatal(_)
        ));
    }

    /// A device that said why is never the silent crash, whatever its last word was. The crash is
    /// retried and its reason replaced with "without saying why", so misreading one loses the only
    /// explanation there was and sends an operator to a log that has nothing.
    ///
    /// The stderr below is constructed rather than observed: no `uiautomator` is known to end a
    /// line this way, and nothing stops one.
    #[test]
    fn a_device_that_explained_itself_is_not_a_crash_however_its_message_ends() {
        let mut explained = fake_failing_dump("java.lang.SecurityException: dump failed:");
        let Attempt::Fatal(e) = dump_once(&mut explained, PREFIX, ample()) else {
            panic!("a device that gave a reason is not a crash that waiting resolves");
        };
        assert!(e.to_string().contains("SecurityException"), "{e}");
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
                        return Err(a_real_timeout_hinted());
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
        assert!(
            e.tool_said()
                .is_some_and(|said| said.contains("No such file")),
            "the read's own reason was replaced: {e}"
        );
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
        dump_until_ready(
            &mut run,
            PREFIX,
            RetryBound::ONCE,
            Duration::ZERO,
            AxDeadline::UNBOUNDED,
        )
        .unwrap();

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
            AxDeadline::UNBOUNDED,
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
        dump_until_ready(
            &mut run,
            PREFIX,
            bound,
            Duration::from_secs(2),
            AxDeadline::UNBOUNDED,
        )
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
        let mut attempts = 0;
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", _] => {
                    attempts += 1;
                    Ok((String::new(), format!("{NOT_READY}\n")))
                }
                ["shell", "cat", _] => Err(never_started_for_want_of_time()),
                _ => Ok((String::new(), String::new())),
            }
        };
        let e = dump_until_ready(
            &mut run,
            PREFIX,
            patient(),
            Duration::ZERO,
            AxDeadline::UNBOUNDED,
        )
        .unwrap_err();

        let msg = e.to_string();
        assert_eq!(
            e.bound(),
            Some(BoundKind::NotStarted),
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
        let xml = dump_until_ready(
            &mut run,
            PREFIX,
            patient(),
            Duration::ZERO,
            AxDeadline::UNBOUNDED,
        )
        .expect("a device that becomes ready within the budget must produce a tree");
        assert_eq!(xml, XML);
    }

    #[test]
    fn a_dump_that_never_becomes_ready_fails_with_the_last_reason() {
        let mut run = fake(usize::MAX);
        let e = dump_until_ready(
            &mut run,
            PREFIX,
            RetryBound::ONCE,
            Duration::ZERO,
            AxDeadline::UNBOUNDED,
        )
        .unwrap_err();
        assert!(e.to_string().contains(NOT_READY), "{e}");
        // The variant, not just the text: `out_of_time` embeds the device's last reason too, so a
        // substring alone passes on both branches — and the two send `wait_for_element` opposite
        // ways, one polling on and one failing at once.
        assert!(
            matches!(e, GlassError::AccessibilityUnavailable(_)),
            "a device that never became ready was reported as a spent caller budget: {e}"
        );
    }

    #[test]
    fn an_adb_failure_is_not_retried() {
        // A device that has gone away will not come back by waiting, and a caller polling
        // with its own timeout must not be held past it.
        let mut attempts = 0;
        let mut run = |argv: &[&str], _deadline: Instant| -> Result<(String, String)> {
            match argv {
                ["shell", "uiautomator", "dump", path] => {
                    attempts += 1;
                    Err(a_failed_call(
                        &["shell", "uiautomator", "dump", path],
                        "device 'emulator-5554' not found",
                    ))
                }
                _ => Ok((String::new(), String::new())),
            }
        };
        let e = dump_until_ready(
            &mut run,
            PREFIX,
            patient(),
            Duration::ZERO,
            AxDeadline::UNBOUNDED,
        )
        .unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");
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
        let xml = dump_until_ready(
            &mut run,
            PREFIX,
            patient(),
            Duration::ZERO,
            AxDeadline::UNBOUNDED,
        )
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
            dump_until_ready(
                &mut run,
                PREFIX,
                patient(),
                Duration::ZERO,
                AxDeadline::UNBOUNDED,
            )
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

    /// Pin the whole verdict, not the variant: what the caller does next comes from the two values
    /// it names, and a field that transformed the text is told apart from one that never received
    /// it only by reading them (glass#363). Twin of the helper in `glass-ios/src/a11y.rs`.
    #[track_caller]
    fn assert_not_applied(outcome: Result<()>, id: u32, requested: &str, observed: Option<&str>) {
        match outcome {
            Err(GlassError::AxValueNotApplied {
                id: got_id,
                requested: req,
                observed: obs,
            }) => assert_eq!(
                (got_id, req.as_str(), obs.as_deref()),
                (id, requested, observed)
            ),
            other => panic!("expected a not-applied verdict, got {other:?}"),
        }
    }

    #[test]
    fn a_cleared_field_reporting_its_hint_reads_as_not_applied() {
        // The cost of judging a clear by "reads back empty", pinned as a decision: this device does
        // empty the field and `uiautomator` does report the hint as its text. Accepting "the value
        // changed" instead would accept a clear that never fired on any field that reformats when it
        // takes focus, which is the false success this check exists to prevent.
        let after = tree_holding(Some("Search settings"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert_not_applied(verify_write(&after, &t, ""), 0, "", Some("Search settings"));
    }

    #[test]
    fn a_field_that_never_changed_is_not_a_successful_write() {
        // The case `verify_write`'s doc is about: the field still holds the old text.
        let after = tree_holding(Some("hello"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert_not_applied(verify_write(&after, &t, "world"), 0, "world", Some("hello"));
    }

    #[test]
    fn an_empty_field_reports_an_empty_read_back_not_a_missing_one() {
        // `axmap` drops an empty value, so the node arrives holding `None` — which the verdict
        // reserves for a reading nobody took. A write that cleared the field and lost its text must
        // say the field is empty, not that it could not be read.
        let after = tree_holding(None);
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert_not_applied(verify_write(&after, &t, "world"), 0, "world", Some(""));
    }

    #[test]
    fn a_partly_typed_write_is_not_a_successful_write() {
        // The reason the rule is an exact match; `typed_text_landed` carries the argument.
        let after = tree_holding(Some("worl"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert_not_applied(verify_write(&after, &t, "world"), 0, "world", Some("worl"));
    }

    #[test]
    fn a_field_that_reformats_what_it_was_given_reports_not_applied() {
        // The cost of the strictness, pinned so it is a decision rather than a surprise: a mask that
        // turns "1234567890" into "(123) 456-7890" reads as not applied. An agent can re-read the
        // tree and see the value; a false success would have it asserting against the wrong text.
        let after = tree_holding(Some("(123) 456-7890"));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert_not_applied(
            verify_write(&after, &t, "1234567890"),
            0,
            "1234567890",
            Some("(123) 456-7890"),
        );
    }

    #[test]
    fn a_write_confirmed_at_a_new_id_is_a_success() {
        // A node grew above the field, so its id moved. The field is still there holding exactly
        // what was asked for, and re-finding it by role and name is what lets that be confirmed
        // instead of refused (glass#359).
        let after = under_a_container(tree_holding(Some("world")));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(verify_write(&after, &t, "world").is_ok());
    }

    #[test]
    fn a_write_confirmed_at_a_new_id_still_checks_what_landed() {
        // Re-finding must not weaken the value check: the field is found, and holds the wrong text.
        let after = under_a_container(tree_holding(Some("worl")));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert_not_applied(verify_write(&after, &t, "world"), 0, "world", Some("worl"));
    }

    #[test]
    fn a_write_whose_screen_was_replaced_says_it_was_typed_but_unconfirmed() {
        // glass#323's other half: the kill can land between the write and the read-back, which
        // is where four of the nine observed refusals came from.
        let err = verify_write(
            &a_different_screen(),
            &target(0, Some("Search"), Some(BOUNDS)),
            "world",
        )
        .unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(err.to_string().contains("the text was typed"), "{err}");
        assert!(err.to_string().contains("replaced"), "{err}");
    }

    #[test]
    fn a_node_that_lost_editability_is_unconfirmed_not_changed() {
        // A caller reading "not dispatched" off this would retype into whatever the field
        // collapsed into.
        let mut after = tree_holding(Some("world"));
        after.root.states.editable = false;
        let t = target(0, Some("Search"), Some(BOUNDS));
        let err = verify_write(&after, &t, "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(err.to_string().contains("editable"), "{err}");
    }

    /// The field with no name at all, which is where role+name identity runs out. An `EditText` with
    /// neither a `content-desc` nor a resource id arrives exactly this way.
    fn nameless(value: Option<&str>) -> (AxTree, AxTarget) {
        let mut tree = tree(AxRole::TextField, None, Some(BOUNDS), true);
        tree.root.value = value.map(Into::into);
        tree.assign_ids();
        (tree, target(0, None, Some(BOUNDS)))
    }

    #[test]
    fn a_named_field_clears_after_being_re_found() {
        // Keying the refusal on "it relocated" refused a clear whenever anything grew above the
        // field; the name is what identifies it, and the name survives the move.
        let after = under_a_container(tree_holding(None));
        let t = target(0, Some("Search"), Some(BOUNDS));
        assert!(verify_write(&after, &t, "").is_ok());
    }

    #[test]
    fn a_nameless_field_cannot_confirm_a_clear_at_its_own_id() {
        // `AxTarget::matches` compares None to None, so the id alone accepts whatever nameless
        // editable renumbering slid onto it — and an empty read-back is exactly what that one shows
        // too. Nothing but the position says the keystrokes reached this field.
        let (after, t) = nameless(None);
        let err = verify_write(&after, &t, "").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(err.to_string().contains("clear"), "{err}");
    }

    #[test]
    fn a_nameless_field_cannot_confirm_a_clear_after_being_re_found_either() {
        // The `Moved` half of the same hole: `matches` compares None to None during the search too,
        // so one nameless editable elsewhere is found by identity alone. Refusing only on `AtId`
        // would let this one through.
        let after = under_a_container(tree(AxRole::TextField, None, Some(BOUNDS), true));
        let t = target(0, None, Some(BOUNDS));
        // Pin the arm: without this the fixture could drift onto AtId and still refuse, for the
        // reason the test above already covers.
        assert!(
            matches!(t.relocate(&after), Located::Moved(n) if n.id == AxNodeId(1)),
            "fixture must reach the Moved arm"
        );
        let err = verify_write(&after, &t, "").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(
            err.to_string().contains("no name"),
            "refused for the identity, not for having relocated: {err}"
        );
    }

    #[test]
    fn a_nameless_field_still_confirms_the_text_it_was_given() {
        // The identity is just as weak here; the typed text is what grounds it instead.
        let (after, t) = nameless(Some("world"));
        assert!(verify_write(&after, &t, "world").is_ok());
    }

    #[test]
    fn losing_editability_outranks_the_clear_refusal() {
        // Both refusals fit a nameless non-editable node. Editability is the specific fact, and a
        // caller told only "an empty field cannot confirm a clear" would go on hunting for a field.
        let (mut after, t) = nameless(None);
        after.root.states.editable = false;
        let err = verify_write(&after, &t, "").unwrap_err();
        assert!(err.to_string().contains("editable"), "{err}");
    }

    #[test]
    fn a_field_drawn_clear_of_the_target_does_not_confirm_the_write() {
        // The navigating tap: an editable is usually named by its resource-id leaf, so
        // `search_src_text` matches on every screen built from that layout, and the next screen's
        // field would otherwise confirm a write the keystrokes landed somewhere else.
        let mut elsewhere = tree_holding(Some("world"));
        elsewhere.root.bounds = Some(AxRect { y: 900, ..BOUNDS });
        let after = under_a_container(elsewhere);
        let t = target(0, Some("Search"), Some(BOUNDS));
        let err = verify_write(&after, &t, "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(err.to_string().contains("drawn clear of where"), "{err}");
    }

    #[test]
    fn a_truncated_tree_cannot_confirm_a_write_against_its_one_match() {
        // One match in the part that was kept is not one match in the tree: the second may be past
        // the cap, which is the refusal a complete tree makes as Ambiguous.
        let mut after = under_a_container(tree_holding(Some("world")));
        after.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 2,
            nodes_walked: 2,
        });
        let t = target(0, Some("Search"), Some(BOUNDS));
        let err = verify_write(&after, &t, "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(err.to_string().contains("truncated at 2 nodes"), "{err}");
    }

    #[test]
    fn an_unreadable_subtree_cannot_confirm_a_write_against_its_one_match() {
        // An unreadable subtree is the other place a second match can hide, and its remedy is
        // not a raised cap, so its message must not collapse into the drifted-element one.
        let mut after = under_a_container(tree_holding(Some("world")));
        after.unreadable = 1;
        let t = target(0, Some("Search"), Some(BOUNDS));
        let err = verify_write(&after, &t, "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(err.to_string().contains("could not be read"), "{err}");
    }

    #[test]
    fn a_dump_path_names_this_process_and_never_repeats() {
        // Two hosts can drive one device, and the file is the only signal a dump succeeded, so
        // a retry must not read one a previous attempt wrote.
        let tag = super::process_tag();
        assert!(
            tag.starts_with(&format!("{}_", std::process::id())),
            "the tag must identify this process, got {tag:?}"
        );
        let first = super::attempt_path("/sdcard/glass_dump");
        assert!(first.contains(tag), "{first}");
        assert_ne!(first, super::attempt_path("/sdcard/glass_dump"));
    }

    #[test]
    #[cfg(unix)]
    fn a_reader_resolves_its_device_once_and_only_among_the_online_ones() {
        use super::AndroidA11y;
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::new(&[(
            "devices",
            Answer::says(
                "List of devices attached\nemulator-5554\tdevice\nemulator-5556\toffline\n",
            ),
        )]);
        let mut reader = AndroidA11y {
            adb: fake.adb().clone(),
            resolved: false,
            warmed: false,
            want_serial: None,
        };

        let bound = reader.ensure_adb().expect("one device is online");
        assert_eq!(bound.serial(), Some("emulator-5554"));

        // Resolved once: a session's reads must not re-ask, or a device appearing mid-session
        // could move the reader to a different one than the platform is driving.
        let again = reader.ensure_adb().expect("already resolved");
        assert_eq!(again.serial(), Some("emulator-5554"));
        assert_eq!(
            fake.calls().iter().filter(|c| *c == "devices").count(),
            1,
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_dump_that_exits_zero_and_explains_itself_on_stderr_is_read_as_a_failure() {
        // `uiautomator dump` reports failure by exiting 0 with its reason on stderr and no file
        // written, so its exit status alone reads as a tree that arrived.
        use super::AndroidA11y;
        use crate::adb::{Answer, FakeAdb};
        use glass_core::Accessibility;

        let fake = FakeAdb::new(&[
            (
                "*uiautomator dump*",
                Answer::warns("ERROR: could not get idle state.\n"),
            ),
            // No file was written, so the read that follows finds nothing.
            ("*shell cat*", Answer::fails("No such file or directory")),
            ("*", Answer::Silent),
        ]);

        let mut reader = AndroidA11y::for_adb(fake.adb().clone());
        let ctx = AxContext {
            pids: vec![],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: AxDeadline::from_millis(300),
        };

        let err = reader
            .snapshot(&ctx)
            .expect_err("a dump that wrote no tree did not serve one");
        // The device's own words, not a complaint about the read that came up empty.
        assert!(
            err.to_string().contains("could not get idle state"),
            "the reason must come from the dump: {err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_reader_binds_to_the_serial_it_was_asked_for_when_several_are_online() {
        // Two online devices is ambiguous on its own; `GLASS_ANDROID_SERIAL` is what resolves it,
        // and it is read when the reader is made rather than when it resolves.
        use super::AndroidA11y;
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::new(&[(
            "devices",
            Answer::says(
                "List of devices attached\nemulator-5554\tdevice\nemulator-5556\tdevice\n",
            ),
        )]);
        let mut reader = AndroidA11y {
            adb: fake.adb().clone(),
            resolved: false,
            warmed: false,
            want_serial: Some("emulator-5556".to_string()),
        };

        let bound = reader.ensure_adb().expect("the named device is online");
        assert_eq!(bound.serial(), Some("emulator-5556"));

        // Without the preference the same listing is refused rather than guessed at.
        let mut blind = AndroidA11y {
            adb: fake.adb().clone(),
            resolved: false,
            warmed: false,
            want_serial: None,
        };
        assert!(blind.ensure_adb().is_err(), "two devices cannot be guessed");
    }

    #[test]
    #[cfg(unix)]
    fn a_reader_handed_a_resolved_client_does_not_go_looking_for_a_device() {
        // The platform has already chosen — possibly a freshly booted AVD that `choose_serial`
        // could not have disambiguated — so asking again could land on a different device.
        use super::AndroidA11y;
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::new(&[("*", Answer::says(""))]);
        let mut reader = AndroidA11y::for_adb(fake.adb().with_serial("emulator-5556"));

        let bound = reader.ensure_adb().expect("already resolved");
        assert_eq!(bound.serial(), Some("emulator-5556"));
        assert!(!fake.called("devices"), "{:?}", fake.calls());
    }

    /// A uiautomator dump of one editable field holding `text`.
    #[cfg(unix)]
    fn one_field_holding(text: &str) -> String {
        format!(
            concat!(
                "<?xml version='1.0'?><hierarchy rotation=\"0\">",
                "<node index=\"0\" text=\"{}\" class=\"android.widget.EditText\" ",
                "package=\"com.example.app\" content-desc=\"Search\" enabled=\"true\" ",
                "focusable=\"true\" focused=\"true\" bounds=\"[100,200][500,300]\" />",
                "</hierarchy>"
            ),
            text
        )
    }

    #[test]
    #[cfg(unix)]
    fn a_write_taps_the_field_clears_it_types_and_waits_for_the_value_to_land() {
        use super::AndroidA11y;
        use crate::adb::{Answer, FakeAdb};
        use glass_core::Accessibility;

        // Old text on the first read-back, new on the second: the IME is still settling, which
        // is when a single read would call a good write failed.
        let before = Answer::says(one_field_holding("hello"));
        let after = Answer::says(one_field_holding("world"));
        let fake = FakeAdb::scripted(&[
            ("*shell cat*", vec![&before, &before, &after]),
            ("*", vec![&Answer::Silent]),
        ]);

        let mut reader = AndroidA11y::for_adb(fake.adb().clone());
        let ctx = AxContext {
            pids: vec![],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: AxDeadline::from_millis(30_000),
        };
        // The synthetic Window root is id 0, so the field is id 1.
        let field = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("Search".into()),
            bounds: Some(AxRect {
                x: 100,
                y: 200,
                width: 400,
                height: 100,
            }),
            value: None,
        };

        reader
            .set_value(&ctx, &field, "world")
            .expect("the value lands on the second read-back");

        // Tapped at the field's centre, cleared, and typed. Order is not asserted here; the
        // read-back below is what proves the field ended up holding the new text and not both.
        assert!(fake.called("input tap 300 250"), "{:?}", fake.calls());
        assert!(fake.called("input text"), "{:?}", fake.calls());
        let cleared = fake
            .calls()
            .iter()
            .any(|c| c.contains("input keyevent") || c.contains("keycombination"));
        assert!(
            cleared,
            "the field must be cleared first: {:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_write_that_never_lands_is_reported_rather_than_assumed() {
        use super::AndroidA11y;
        use crate::adb::{Answer, FakeAdb};
        use glass_core::Accessibility;

        let stuck = Answer::says(one_field_holding("hello"));
        let fake =
            FakeAdb::scripted(&[("*shell cat*", vec![&stuck]), ("*", vec![&Answer::Silent])]);

        let mut reader = AndroidA11y::for_adb(fake.adb().clone());
        let ctx = AxContext {
            pids: vec![],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: AxDeadline::from_millis(30_000),
        };
        let field = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("Search".into()),
            bounds: Some(AxRect {
                x: 100,
                y: 200,
                width: 400,
                height: 100,
            }),
            value: None,
        };

        let err = reader
            .set_value(&ctx, &field, "world")
            .expect_err("a field still holding the old text has not taken the write");
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { id: 1, requested, observed }
                if requested == "world" && observed.as_deref() == Some("hello")),
            "{err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_verdict_names_the_last_read_back_not_the_first() {
        // The read-back retries because a field can commit a frame or two late, so the attempts can
        // disagree: a first read caught mid-write and a second holding what the field settled on.
        // Reporting the first would tell the caller the write never arrived when it arrived
        // transformed — the misdiagnosis the two values exist to prevent.
        use super::AndroidA11y;
        use crate::adb::{Answer, FakeAdb};
        use glass_core::Accessibility;

        // Three reads, in order: the pre-write locate, then the two verify attempts that disagree.
        let locate = Answer::says(one_field_holding("old"));
        let mid = Answer::says(one_field_holding("hel"));
        let settled = Answer::says(one_field_holding("Hello"));
        let fake = FakeAdb::scripted(&[
            ("*shell cat*", vec![&locate, &mid, &settled]),
            ("*", vec![&Answer::Silent]),
        ]);

        let mut reader = AndroidA11y::for_adb(fake.adb().clone());
        let ctx = AxContext {
            pids: vec![],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: AxDeadline::from_millis(30_000),
        };
        let field = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("Search".into()),
            bounds: Some(AxRect {
                x: 100,
                y: 200,
                width: 400,
                height: 100,
            }),
            value: None,
        };

        let err = reader
            .set_value(&ctx, &field, "hello")
            .expect_err("neither read holds the requested text");
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { observed, .. }
                if observed.as_deref() == Some("Hello")),
            "{err}"
        );
    }

    #[test]
    fn a_failed_read_back_reports_the_write_as_unconfirmed() {
        // The read runs after the keystrokes went out, so a dump failure here must be a verdict
        // `set_value_failed_after_writing` accepts — otherwise the session keeps the value it cached
        // for a write that already landed and refuses the retry as drift.
        let t = target(0, Some("Search"), Some(BOUNDS));
        let err = read_back_failed(
            &t,
            &GlassError::AccessibilityUnavailable("no window roots".into()),
        );
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(err.set_value_failed_after_writing(), "{err}");
        assert!(err.to_string().contains("no window roots"), "{err}");
    }

    #[test]
    fn an_id_past_the_end_still_confirms_a_landed_write() {
        // The mirror image of the renumbering case, and not the keyboard's doing — that measurably
        // does not shift ids here (see `verify_write`'s doc). A tap that navigates can land on a
        // smaller window, leaving the field's old id past the end of the tree: nothing resolves
        // there, but the field itself carries the identity and holds the text.
        let after = tree_holding(Some("world"));
        let t = target(7, Some("Search"), Some(BOUNDS));
        assert!(verify_write(&after, &t, "world").is_ok());
    }

    #[test]
    fn a_truncated_read_back_says_so_rather_than_blaming_drift() {
        // No node here carries the target's role and name, so a complete tree would call this
        // Gone — the cap is what keeps it Unproven, and the cap is what tells a caller to raise
        // max_nodes, so the message must keep naming it.
        let mut after = tree(AxRole::Label, Some("No Results"), Some(BOUNDS), false);
        after.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 1,
            nodes_walked: 1,
        });
        let t = target(7, Some("Search"), Some(BOUNDS));
        let err = verify_write(&after, &t, "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(7, _)), "{err}");
        assert!(err.to_string().contains("truncated at 1 nodes"), "{err}");
    }

    #[test]
    fn two_matching_fields_refuse_and_say_how_many() {
        // The target's own id must land on something else first — a node still holding it would
        // take the AtId fast path and never learn a second candidate exists.
        let mut container = tree(AxRole::Group, None, Some(BOUNDS), false);
        let mut a = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true);
        a.root.value = Some("world".into());
        let mut b = tree(AxRole::TextField, Some("Search"), Some(BOUNDS), true);
        b.root.value = Some("world".into());
        container.root.children = vec![a.root, b.root];
        container.assign_ids();
        let t = target(0, Some("Search"), Some(BOUNDS));
        let err = verify_write(&container, &t, "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(0, _)), "{err}");
        assert!(
            err.to_string().contains("2 elements now match"),
            "names how many, not just some digit: {err}"
        );
    }

    #[test]
    fn an_incomplete_tree_does_not_claim_the_element_vanished() {
        // `unreadable` is independent of the caps, and equally unable to support "replaced".
        let mut after = tree(AxRole::Label, Some("No Results"), Some(BOUNDS), false);
        after.unreadable = 1;
        let t = target(9, Some("Search"), Some(BOUNDS));
        let err = verify_write(&after, &t, "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(9, _)), "{err}");
        assert!(
            !err.to_string().contains("replaced"),
            "an incomplete read must not assert the screen changed: {err}"
        );
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
