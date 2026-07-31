//! UI Automation change notifications, so a wait stops re-walking a tree that has not changed.
//!
//! A subscription owns its thread for its whole life. The reader spawns a thread per snapshot and
//! lets it die; a registration cannot work that way, because it belongs to the COM apartment the
//! thread initialized and has to outlive any single read.
//!
//! Handlers are invoked by UIA on its own RPC threads, not this one — which is why nothing but a
//! unit crosses the channel (see [`Notify`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use glass_core::{AxContext, ChangeSignal, ChangeWait};
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

use crate::mapping::WatchedProperty;

/// The one place [`WatchedProperty`] becomes a `uiautomation` type.
///
/// Exhaustive: a new `WatchedProperty` fails to compile here until it is given a UIA property to
/// register, which is what stops a condition naming a property the subscription never asks for.
const fn uia_property(p: WatchedProperty) -> UIProperty {
    match p {
        WatchedProperty::Name => UIProperty::Name,
        WatchedProperty::HasKeyboardFocus => UIProperty::HasKeyboardFocus,
        WatchedProperty::IsEnabled => UIProperty::IsEnabled,
        WatchedProperty::IsOffscreen => UIProperty::IsOffscreen,
        WatchedProperty::Value => UIProperty::ValueValue,
        WatchedProperty::ExpandCollapseState => UIProperty::ExpandCollapseExpandCollapseState,
        WatchedProperty::SelectionItemIsSelected => UIProperty::SelectionItemIsSelected,
        WatchedProperty::ToggleState => UIProperty::ToggleToggleState,
    }
}

/// The properties registered on the window. Built from [`WatchedProperty::ALL`] rather than
/// listed again here, so there is no second list to fall out of step.
fn watched() -> [UIProperty; WatchedProperty::ALL.len()] {
    WatchedProperty::ALL.map(uia_property)
}

/// How long to wait for the pump to establish both registrations. Registration cost scales with
/// tree size — measured 38ms + 17ms on a 1500-node window — and it is the *caller's* wait budget
/// being spent, so this is bounded generously but never unbounded: failing to subscribe must cost
/// a poll, not a hang.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The per-call limit `bounded_automation` sets, in milliseconds (the unit
/// `IUIAutomation2::SetTransactionTimeout` itself takes). UIA's default, measured on a Windows
/// box, is 20000ms, so this is a deliberate 10x tightening — still generous against the slowest
/// measured call, registration's 38ms + 17ms on a 1500-node window. It bounds one call, not the
/// whole prelude: `subscribe`'s handshake makes up to three, so three slow-but-succeeding calls
/// can still expire `SUBSCRIBE_TIMEOUT` first. That case is benign — `subscribe` returns `None`
/// and the pump, seeing `running == false`, tears down.
const TRANSACTION_TIMEOUT_MS: u32 = 2_000;

/// How long the pump sleeps between `running` checks — a cadence, not the shutdown bound. A
/// dropped signal is *observed* within this long plus at most one bounded liveness probe, and
/// teardown then makes two more bounded calls, so shutdown is bounded by that sum
/// (`SHUTDOWN_CHECK` + 3 × `TRANSACTION_TIMEOUT_MS` at worst), not by this constant alone.
const SHUTDOWN_CHECK: Duration = Duration::from_millis(250);

/// How often the pump confirms a *quiet* registration is still delivering, by re-resolving the
/// window. Far slower than `SHUTDOWN_CHECK` on purpose: this is a cross-process call into the
/// target app's own UIA provider, and running it at shutdown-check cadence would spend on
/// liveness exactly the cost the subscription exists to avoid — and would gate the shutdown-check
/// promptness above behind a live call on every tick instead of one every couple of seconds.
/// Nothing else notices a dead registration — the app exiting, or destroying and recreating its
/// top-level window, stops events without disconnecting either sender — so without this check
/// `wait` would report `Quiet` for the caller's entire budget instead of `Unusable`, which
/// licenses skipping a re-read of a tree that is actually changing. A registration that has
/// delivered recently skips this probe entirely (see `Notify::delivered`), so this cadence is
/// only ever spent on a subscription that has actually gone quiet.
const LIVENESS_CHECK: Duration = Duration::from_secs(2);

/// Both handlers report the same thing — that *something* changed — because that is all the wait
/// needs; it re-reads the tree itself.
///
/// A `UIElement` must never be sent through this channel. It is apartment-affine and these
/// handlers run on UIA's RPC threads, not the pump's. Reporting *what* changed is the obvious
/// improvement and the one that would break it.
struct Notify {
    tx: SyncSender<()>,
    /// Set whenever a handler fires, so the pump can skip a `LIVENESS_CHECK` probe against a
    /// subscription that just proved itself alive. Relaxed is enough: this is a hint the pump
    /// reads on its own cadence, and observing it late costs at most one skipped-then-redone
    /// probe cycle, never a wrong answer.
    delivered: Arc<AtomicBool>,
}

impl Notify {
    /// Shared by both handler impls below, so `delivered` and `tx` stay in lockstep — a change
    /// that reaches one and not the other would make the liveness probe distrust an app that is
    /// actually still talking.
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

/// A [`ChangeSignal`] fed by UIA event handlers registered on a background thread.
pub(crate) struct UiaChanges {
    rx: Receiver<()>,
    live: bool,
    /// Cleared on drop to stop the pump, which is what removes the registrations. Without it the
    /// target app's UIA provider keeps doing work for a subscription nobody holds — degrading the
    /// app being driven and glass's own walks through it.
    running: Arc<AtomicBool>,
}

impl Drop for UiaChanges {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl ChangeSignal for UiaChanges {
    fn wait(&mut self, timeout: Duration) -> ChangeWait {
        if !self.live {
            return ChangeWait::Unusable;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(()) => {
                // One logical change delivers several events — a control and its text peer, and a
                // structure event alongside a property event for one insertion. Drain: a burst is
                // one reason to re-read, not one re-read each.
                while self.rx.try_recv().is_ok() {}
                ChangeWait::Changed
            }
            Err(RecvTimeoutError::Timeout) => ChangeWait::Quiet,
            Err(RecvTimeoutError::Disconnected) => {
                self.live = false;
                ChangeWait::Unusable
            }
        }
    }
}

/// Subscribe to changes for the window in `ctx`, or `None` if no subscription could be
/// established — the caller then polls exactly as it did before.
pub(crate) fn subscribe(ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
    let ctx = ctx.clone();
    // Capacity 1: the receiver only needs to learn that *something* changed, and a full channel
    // already says that, so a chatty app cannot grow a backlog here.
    let (tx, rx) = sync_channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let pump_running = running.clone();
    std::thread::spawn(move || pump(&ctx, tx, &ready_tx, &pump_running));
    // Wait for the registrations to be in place before returning: the caller reads the tree the
    // moment it has a signal, and a change landing before registration would be lost.
    match ready_rx.recv_timeout(SUBSCRIBE_TIMEOUT) {
        Ok(true) => Some(Box::new(UiaChanges {
            rx,
            live: true,
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
/// — registration, the liveness probe, both teardown calls — at `TRANSACTION_TIMEOUT_MS`. Without
/// that bound a wedged-but-alive provider (busy, not exited) can hold the pump inside a
/// synchronous COM call indefinitely, since such a call has no clean cancellation.
///
/// The bound belongs to the object, not to the thread, so the pump must use *this* object for
/// everything. It cannot come from `UIAutomation::new()`: that creates a `CUIAutomation` instance,
/// and only the separate `CUIAutomation8` coclass implements `IUIAutomation2`, the interface
/// carrying the knob (Windows 8.1+, so it exists everywhere glass runs). Casting the former to it
/// fails with `E_NOINTERFACE`, measured on a Windows box. COM is initialized here rather than by
/// calling `UIAutomation::new()` for its `CoInitializeEx` side effect, which would leave a second,
/// unbounded automation object with nothing to do.
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
) {
    // Initializes COM (MTA) on this thread, which the registrations below belong to. It is also
    // the only automation object this function may use: the transaction bound lives on the object,
    // so a call made through any other one is unbounded. Failing to build it is a subscribe
    // failure like every other early exit here — that costs the caller a resumed poll, where
    // continuing unbounded would silently drop the one property this exists to add.
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
            // A handler fired since the last check, so the registration just proved itself
            // alive — probing it now would be pure cost for zero information. Reset the
            // cadence from here too, so a continuously busy app is never probed at all.
            last_liveness_check = Instant::now();
            continue;
        }
        if last_liveness_check.elapsed() < LIVENESS_CHECK {
            continue;
        }
        last_liveness_check = Instant::now();
        // Deliberately fails toward `Unusable`, not toward retrying: a transient read failure
        // here costs the caller one resumed poll, exactly today's behaviour without a
        // subscription at all. Reporting `Quiet` on the same failure would be silently wrong
        // instead of merely conservative, so a failed probe ends the pump rather than being
        // retried — the teardown below still runs on the way out.
        if crate::reader::find_app_window(&automation, ctx).is_err() {
            break;
        }
    }

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
    #[test]
    fn no_event_within_the_timeout_reports_quiet() {
        let (_tx, rx) = sync_channel(1);
        let mut changes = UiaChanges {
            rx,
            live: true,
            running: Arc::new(AtomicBool::new(true)),
        };
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
        let mut changes = UiaChanges {
            rx,
            live: true,
            running: Arc::new(AtomicBool::new(true)),
        };
        assert_eq!(changes.wait(Duration::from_millis(20)), ChangeWait::Changed);
        assert!(
            changes.rx.try_recv().is_err(),
            "a queued event survived the drain, so the next wait would re-report a stale change"
        );
    }

    /// The subscription itself ended — the stream, not merely the app, went quiet — so the caller
    /// must resume polling rather than trust a signal that can no longer speak.
    #[test]
    fn a_disconnected_channel_reports_unusable() {
        let (tx, rx) = sync_channel::<()>(1);
        drop(tx);
        let mut changes = UiaChanges {
            rx,
            live: true,
            running: Arc::new(AtomicBool::new(true)),
        };
        assert_eq!(
            changes.wait(Duration::from_millis(20)),
            ChangeWait::Unusable
        );
    }

    /// `live` is the sticky half of the contract: once tripped, `wait` must return `Unusable`
    /// without consulting the channel again — proven here by leaving a message the channel would
    /// otherwise happily report as `Changed`.
    #[test]
    fn once_live_is_false_wait_reports_unusable_even_with_a_message_pending() {
        let (tx, rx) = sync_channel(1);
        tx.send(()).unwrap();
        let mut changes = UiaChanges {
            rx,
            live: false,
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
        let (_tx, rx) = sync_channel(1);
        let running = Arc::new(AtomicBool::new(true));
        let changes = UiaChanges {
            rx,
            live: true,
            running: running.clone(),
        };
        drop(changes);
        assert!(!running.load(Ordering::Relaxed));
    }
}
