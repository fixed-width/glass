//! UI Automation change notifications, so a wait stops re-walking a tree that has not changed.
//!
//! A registration belongs to the COM apartment its thread initialized, so the pump thread outlives
//! any single read — unlike the reader's per-snapshot threads.
//!
//! Handlers run on UIA's own RPC threads, which is why nothing but a unit crosses the channel
//! (see [`Notify`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use glass_core::{AxContext, ChangeSignal, ChangeWait, ElementCondition};
use uiautomation::events::{
    CustomPropertyChangedEventHandler, CustomStructureChangedEventHandler,
    UIPropertyChangedEventHandler, UIStructureChangeEventHandler,
};
use uiautomation::types::{StructureChangeType, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::{CUIAutomation8, IUIAutomation, IUIAutomation2};

use crate::mapping::{SELECTOR_PROPERTIES, WatchedProperty, announcing_property};

/// The one place [`WatchedProperty`] becomes a `uiautomation` type.
///
/// Exhaustive, so a new `WatchedProperty` fails to compile here. An arm names the property that
/// actually gets registered whenever [`watched`] reaches it, so a mis-paired arm registers the
/// wrong property; `each_watched_property_converts_to_the_uia_property_with_its_id` is what
/// catches that.
const fn uia_property(p: WatchedProperty) -> UIProperty {
    match p {
        WatchedProperty::Name => UIProperty::Name,
        WatchedProperty::HasKeyboardFocus => UIProperty::HasKeyboardFocus,
        WatchedProperty::IsEnabled => UIProperty::IsEnabled,
        WatchedProperty::IsOffscreen => UIProperty::IsOffscreen,
        WatchedProperty::Value => UIProperty::ValueValue,
        WatchedProperty::RangeValue => UIProperty::RangeValueValue,
        WatchedProperty::ExpandCollapseState => UIProperty::ExpandCollapseExpandCollapseState,
        WatchedProperty::SelectionItemIsSelected => UIProperty::SelectionItemIsSelected,
        WatchedProperty::ToggleState => UIProperty::ToggleToggleState,
    }
}

/// The whole of what this subscription asks UIA for: the property announcing each
/// [`ElementCondition`], plus the properties selectors match on ([`SELECTOR_PROPERTIES`]).
///
/// Derived rather than hand-listed, so `mapping.rs`'s declarations are the thing that decides what
/// is registered — a condition whose announcing property is wrong now registers the wrong
/// property, rather than being a claim no code reads.
fn watched() -> Vec<UIProperty> {
    let mut watched: Vec<WatchedProperty> = Vec::new();
    for p in ElementCondition::ALL
        .into_iter()
        .filter_map(announcing_property)
        .chain(SELECTOR_PROPERTIES)
    {
        // Duplicates are the rule here, not the exception — a condition and its inverse share an
        // announcing property, and a selector property can announce a condition too — and they are
        // not adjacent, so `Vec::dedup` would leave some behind. Order is not preserved for
        // anything's sake: this is the set of properties to register, and nothing reads the order
        // it is built in.
        if !watched.contains(&p) {
            watched.push(p);
        }
    }
    watched.into_iter().map(uia_property).collect()
}

/// How long to wait for the pump to establish both registrations, which cost 38ms + 17ms on a
/// 1500-node window and scale with tree size. Spent from the caller's budget, so it is a visible
/// ceiling and a wider one than the Linux reader's 2s: against a provider that never finishes
/// registering, `timeout_ms: 500` returns in about 3s.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The per-call limit `bounded_automation` sets, in the milliseconds
/// `IUIAutomation2::SetTransactionTimeout` takes. UIA's default is 20000ms, measured on a Windows
/// box. It bounds one call, not the prelude: `subscribe`'s handshake makes up to three, so three
/// slow-but-succeeding calls can still expire `SUBSCRIBE_TIMEOUT` first, which is benign.
const TRANSACTION_TIMEOUT_MS: u32 = 2_000;

/// How long the pump sleeps between `running` checks — a cadence, not the shutdown bound. A
/// dropped signal is observed within this long plus at most one bounded liveness probe, and
/// teardown makes two more, so shutdown is bounded by `SHUTDOWN_CHECK` + 3 ×
/// `TRANSACTION_TIMEOUT_MS` at worst.
const SHUTDOWN_CHECK: Duration = Duration::from_millis(250);

/// How often the pump confirms a *quiet* registration still delivers, by re-resolving the window.
/// Far slower than `SHUTDOWN_CHECK` because it is a cross-process call into the target app's own
/// UIA provider. Nothing else notices a dead registration: the app exiting, or destroying and
/// recreating its top-level window, stops events without disconnecting either sender, so `wait`
/// would report `Quiet` for the caller's entire budget instead of `Unusable`. What reaches `wait`
/// is the pump clearing `alive` on its way out, not the senders — UIA holds those inside the
/// registered handlers until both `remove_*` calls succeed, which is least likely exactly when the
/// window has stopped resolving. A registration that delivered recently skips the probe entirely
/// (see `Notify::delivered`).
const LIVENESS_CHECK: Duration = Duration::from_secs(2);

/// Both handlers report only that *something* changed; the wait re-reads the tree itself.
///
/// A `UIElement` must never be sent through this channel — it is apartment-affine and these
/// handlers run on UIA's RPC threads. Reporting *what* changed is the obvious improvement and the
/// one that would break it.
struct Notify {
    tx: SyncSender<()>,
    /// Set whenever a handler fires, so the pump skips a `LIVENESS_CHECK` probe against a
    /// subscription that just proved itself alive. Relaxed suffices — observing it late costs one
    /// probe cycle, never a wrong answer.
    delivered: Arc<AtomicBool>,
}

impl Notify {
    /// Shared by both handler impls so `delivered` and `tx` stay in lockstep — updating one alone
    /// would make the liveness probe distrust an app that is still talking.
    fn record_and_forward(&self) {
        self.delivered.store(true, Ordering::Relaxed);
        // A full channel already carries "something changed", so dropping this one loses nothing.
        let _ = self.tx.try_send(());
    }
}

impl CustomStructureChangedEventHandler for Notify {
    fn handle(
        &self,
        _sender: &UIElement,
        _change_type: StructureChangeType,
        _runtime_id: Option<&[i32]>,
    ) -> uiautomation::Result<()> {
        self.record_and_forward();
        Ok(())
    }
}

impl CustomPropertyChangedEventHandler for Notify {
    fn handle(
        &self,
        _sender: &UIElement,
        _property: UIProperty,
        _new_value: Variant,
    ) -> uiautomation::Result<()> {
        self.record_and_forward();
        Ok(())
    }
}

/// Whether the pump can still be heard from. Two states rather than a receiver plus a flag, so
/// "ended, but still holding a channel to read" is a state that cannot be built.
enum Signaling {
    Live {
        rx: Receiver<()>,
        /// Set while the pump can still deliver, cleared on its way out (see [`ClearOnExit`]). A
        /// dead pump does not reliably disconnect the channel — UIA holds both senders inside COM
        /// objects until the two `remove_*` calls succeed, against a window that has usually just
        /// stopped resolving — so this, not `RecvTimeoutError::Disconnected`, is what tells `wait`
        /// the pump is gone.
        alive: Arc<AtomicBool>,
    },
    Ended,
}

/// A [`ChangeSignal`] fed by UIA event handlers registered on a background thread.
pub(crate) struct UiaChanges {
    signaling: Signaling,
    /// Cleared on drop to stop the pump, which is what removes the registrations. Leaked, the
    /// target app's provider keeps doing work for a subscription nobody holds. Written by this,
    /// the consumer side, and read by the pump — the opposite direction from `alive`, which is why
    /// the two are separate flags and neither side reads the one it writes.
    running: Arc<AtomicBool>,
}

impl Drop for UiaChanges {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl ChangeSignal for UiaChanges {
    fn wait(&mut self, timeout: Duration) -> ChangeWait {
        let outcome = match &self.signaling {
            Signaling::Ended => ChangeWait::Unusable,
            // `alive` before the channel: once the pump has gone, a queued message is a stale
            // change and the caller re-reads on `Unusable` anyway.
            Signaling::Live { alive, .. } if !alive.load(Ordering::Relaxed) => ChangeWait::Unusable,
            Signaling::Live { rx, .. } => match rx.recv_timeout(timeout) {
                Ok(()) => {
                    // One logical change delivers several events — a control and its text peer, a
                    // structure event alongside a property event. A burst is one reason to re-read.
                    while rx.try_recv().is_ok() {}
                    ChangeWait::Changed
                }
                Err(RecvTimeoutError::Timeout) => ChangeWait::Quiet,
                Err(RecvTimeoutError::Disconnected) => ChangeWait::Unusable,
            },
        };
        if outcome == ChangeWait::Unusable {
            // Drops the receiver with it, so a signal that has answered `Unusable` once has
            // nothing left to answer anything else from.
            self.signaling = Signaling::Ended;
        }
        outcome
    }
}

/// Subscribe to changes for the window in `ctx`, or `None` if no subscription could be
/// established — the caller then polls exactly as it did before.
pub(crate) fn subscribe(ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
    let ctx = ctx.clone();
    // Capacity 1: a full channel already says "something changed", so a chatty app grows no
    // backlog here.
    let (tx, rx) = sync_channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let alive = Arc::new(AtomicBool::new(true));
    let pump_running = running.clone();
    let pump_alive = alive.clone();
    std::thread::spawn(move || pump(&ctx, tx, &ready_tx, &pump_running, &pump_alive));
    // Wait for the registrations to be in place before returning: the caller reads the tree the
    // moment it has a signal, and a change landing before registration would be lost.
    match ready_rx.recv_timeout(SUBSCRIBE_TIMEOUT) {
        Ok(true) => Some(Box::new(UiaChanges {
            signaling: Signaling::Live { rx, alive },
            running,
        })),
        // Including the timeout: the pump may still be starting, and nothing else would ever tell
        // it to stop.
        _ => {
            running.store(false, Ordering::Relaxed);
            None
        }
    }
}

/// Creates the automation object the pump uses, bounding every cross-process call made through it
/// at `TRANSACTION_TIMEOUT_MS`. Unbounded, a wedged-but-alive provider holds the pump inside a
/// synchronous COM call indefinitely, since such a call has no clean cancellation.
///
/// The bound belongs to the object, so the pump must use *this* one for everything. It cannot come
/// from `UIAutomation::new()`: that creates a `CUIAutomation` instance, and only the separate
/// `CUIAutomation8` coclass implements `IUIAutomation2`, which carries the knob (Windows 8.1+).
/// Casting the former to it fails with `E_NOINTERFACE`, measured on a Windows box. COM is
/// initialized here rather than via `UIAutomation::new()`'s side effect, which would leave a
/// second, unbounded automation object with nothing to do.
#[allow(unsafe_code)]
fn bounded_automation() -> windows::core::Result<UIAutomation> {
    // SAFETY: `CoInitializeEx` must run on this thread before `CoCreateInstance`, which this
    // ordering gives; all three take only scalars and `'static` constants and borrow nothing.
    // `CoCreateInstance` hands back an owned interface pointer that `windows` wraps, so there is
    // nothing to alias or keep alive across the calls, and `SetTransactionTimeout` sets a plain
    // `u32` millisecond value on that pointer with no output.
    let automation = unsafe {
        // MTA, matching what `UIAutomation::new()` establishes; the registrations belong to it.
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let automation: IUIAutomation2 = CoCreateInstance(&CUIAutomation8, None, CLSCTX_ALL)?;
        automation.SetTransactionTimeout(TRANSACTION_TIMEOUT_MS)?;
        automation
    };
    // A static upcast, not a `QueryInterface`: `IUIAutomation2` extends `IUIAutomation`, which is
    // the interface `uiautomation` wraps and drives.
    Ok(UIAutomation::from(IUIAutomation::from(automation)))
}

/// Clears an `AtomicBool` however its scope ends: an early return, a normal one, or a panic.
struct ClearOnExit<'a>(&'a AtomicBool);

impl Drop for ClearOnExit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Register both handlers on the window and hold them until the signal is dropped.
///
/// Every early return either registered nothing or removes what it registered. A half-registered
/// subscription that reports failure is the worst case: the caller polls, and the provider still
/// pays for the handler nobody will remove.
fn pump(
    ctx: &AxContext,
    tx: SyncSender<()>,
    ready: &std::sync::mpsc::Sender<bool>,
    running: &AtomicBool,
    alive: &AtomicBool,
) {
    // Whichever way this function ends — a failed prelude, a dropped signal, a failed liveness
    // probe, a panic — `wait` learns the pump is gone. Dropped explicitly before teardown below.
    let alive = ClearOnExit(alive);
    // Initializes COM (MTA) on this thread and is the only automation object this function may
    // use — the transaction bound lives on the object, so a call through any other is unbounded.
    // Failing to build it is a subscribe failure like every other early exit: that costs the caller
    // a resumed poll, where continuing unbounded would drop the property this exists to add.
    let Ok(automation) = bounded_automation() else {
        let _ = ready.send(false);
        return;
    };
    let Ok(window) = crate::reader::find_app_window(&automation, ctx) else {
        let _ = ready.send(false);
        return;
    };

    let delivered = Arc::new(AtomicBool::new(false));
    let structure: UIStructureChangeEventHandler = Notify {
        tx: tx.clone(),
        delivered: delivered.clone(),
    }
    .into();
    if automation
        .add_structure_changed_event_handler(&window, TreeScope::Subtree, None, &structure)
        .is_err()
    {
        let _ = ready.send(false);
        return;
    }

    let property: UIPropertyChangedEventHandler = Notify {
        tx,
        delivered: delivered.clone(),
    }
    .into();
    if automation
        .add_property_changed_event_handler(
            &window,
            TreeScope::Subtree,
            None,
            &property,
            &watched(),
        )
        .is_err()
    {
        let _ = automation.remove_structure_changed_event_handler(&window, &structure);
        let _ = ready.send(false);
        return;
    }
    let _ = ready.send(true);

    let mut last_liveness_check = Instant::now();
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(SHUTDOWN_CHECK);
        if delivered.swap(false, Ordering::Relaxed) {
            // A handler fired since the last check, so the registration just proved itself alive.
            // Resetting the cadence here means a continuously busy app is never probed at all.
            last_liveness_check = Instant::now();
            continue;
        }
        if last_liveness_check.elapsed() < LIVENESS_CHECK {
            continue;
        }
        last_liveness_check = Instant::now();
        // Fails toward `Unusable`, not toward retrying: a transient read failure costs the caller
        // one resumed poll, where reporting `Quiet` on it would be silently wrong. Teardown below
        // still runs on the way out.
        if crate::reader::find_app_window(&automation, ctx).is_err() {
            break;
        }
    }

    // Before the removes, not after: each is a cross-process call bounded at seconds, and a `wait`
    // running through that window must already read `Unusable` rather than `Quiet`.
    drop(alive);
    let _ = automation.remove_structure_changed_event_handler(&window, &structure);
    let _ = automation.remove_property_changed_event_handler(&window, &property);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversion is exhaustive, so nothing can be *missing* — but an arm can still name the
    /// wrong `UIProperty`, and `IsOffscreen` reading as `IsEnabled` would silently register the
    /// wrong thing. Comparing ids is what catches a mis-paired arm.
    #[test]
    fn each_watched_property_converts_to_the_uia_property_with_its_id() {
        for p in WatchedProperty::ALL {
            assert_eq!(
                uia_property(p) as i32 as u32,
                p.id(),
                "{p:?} converts to a UIProperty with a different id"
            );
        }
    }

    /// Keeps the per-call bound meaningful against the handshake budget: inverted, even a single
    /// slow call would expire the subscribe handshake before reaching its own limit, leaving no
    /// way to tell "never registered" from "registering, just slowly". Calls in sequence can
    /// still outrun the handshake — that is the prelude's shape, not something this pins.
    #[test]
    fn transaction_timeout_is_below_the_subscribe_timeout() {
        assert!(Duration::from_millis(u64::from(TRANSACTION_TIMEOUT_MS)) < SUBSCRIBE_TIMEOUT);
    }

    /// The pump's skip decision (`delivered.swap` in its loop) needs a live registration on a
    /// real COM thread to exercise, so it is not unit-testable here — only `Notify` itself is:
    /// this pins that firing a handler both marks `delivered` (what lets the pump skip a probe)
    /// and still forwards to `tx` (what `UiaChanges::wait` reads), so neither regresses alone.
    #[test]
    fn a_fired_handler_marks_delivered_and_still_forwards() {
        let (tx, rx) = sync_channel(1);
        let delivered = Arc::new(AtomicBool::new(false));
        let notify = Notify {
            tx,
            delivered: delivered.clone(),
        };
        notify.record_and_forward();
        assert!(delivered.load(Ordering::Relaxed));
        assert!(rx.try_recv().is_ok());
    }

    /// `Quiet` is what licenses a caller to skip re-reading the tree (see `ChangeWait`) — this
    /// pins the case where that licence is actually earned: nothing arrived, and the signal is
    /// still trustworthy.
    /// Builds the signal the way `subscribe` does, with the pump still running.
    fn live_changes(rx: Receiver<()>) -> UiaChanges {
        UiaChanges {
            signaling: Signaling::Live {
                rx,
                alive: Arc::new(AtomicBool::new(true)),
            },
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    #[test]
    fn no_event_within_the_timeout_reports_quiet() {
        let (_tx, rx) = sync_channel(1);
        let mut changes = live_changes(rx);
        assert_eq!(changes.wait(Duration::from_millis(20)), ChangeWait::Quiet);
    }

    /// A burst must cost the caller one re-read, not one per queued event — a control and its
    /// text peer, say, firing together. The return value alone can't tell a drain from a channel
    /// that only ever held one message, so this also asserts the channel is empty afterward.
    #[test]
    fn a_burst_of_events_reports_changed_once_and_leaves_the_channel_drained() {
        // Capacity here is a test convenience for simulating several pending events at once;
        // production bounds the real channel to 1 for an unrelated reason (backlog growth).
        let (tx, rx) = sync_channel(4);
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        let mut changes = live_changes(rx);
        assert_eq!(changes.wait(Duration::from_millis(20)), ChangeWait::Changed);
        let Signaling::Live { rx, .. } = &changes.signaling else {
            unreachable!("a delivered change leaves the signal live")
        };
        assert!(
            rx.try_recv().is_err(),
            "a queued event survived the drain, so the next wait would re-report a stale change"
        );
    }

    /// The subscription itself ended — the stream, not merely the app, went quiet — so the caller
    /// must resume polling rather than trust a signal that can no longer speak.
    #[test]
    fn a_disconnected_channel_reports_unusable() {
        let (tx, rx) = sync_channel::<()>(1);
        drop(tx);
        let mut changes = live_changes(rx);
        assert_eq!(
            changes.wait(Duration::from_millis(20)),
            ChangeWait::Unusable
        );
    }

    /// `alive` carries the opposite direction to `running`: the pump telling the signal it has
    /// gone. It has to be read *before* the channel, because the channel is the unreliable half
    /// here — UIA holds both senders inside the registered handlers, so a pump that exited because
    /// its window stopped resolving can leave them undropped indefinitely, and every `wait` would
    /// keep reporting `Quiet` for a subscription that will never deliver again. A message left
    /// pending proves the check is not merely the `Disconnected` arm wearing a different name.
    #[test]
    fn once_alive_is_cleared_wait_reports_unusable_even_with_a_message_pending() {
        let (tx, rx) = sync_channel(1);
        tx.send(()).unwrap();
        let mut changes = UiaChanges {
            signaling: Signaling::Live {
                rx,
                alive: Arc::new(AtomicBool::new(false)),
            },
            running: Arc::new(AtomicBool::new(true)),
        };
        assert_eq!(
            changes.wait(Duration::from_millis(20)),
            ChangeWait::Unusable
        );
    }

    /// `running` is the only signal that tells the pump to stop and remove its registrations; if
    /// `Drop` stopped setting it, the pump — and the registrations it holds on the target app's
    /// UIA provider — would outlive every wait that created it.
    #[test]
    fn dropping_the_signal_clears_running() {
        let running = Arc::new(AtomicBool::new(true));
        let changes = UiaChanges {
            signaling: Signaling::Ended,
            running: running.clone(),
        };
        drop(changes);
        assert!(!running.load(Ordering::Relaxed));
    }
}
