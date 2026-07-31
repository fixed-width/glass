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
use std::time::Duration;

use glass_core::{AxContext, ChangeSignal, ChangeWait};
use uiautomation::events::{
    CustomPropertyChangedEventHandler, CustomStructureChangedEventHandler,
    UIPropertyChangedEventHandler, UIStructureChangeEventHandler,
};
use uiautomation::types::{StructureChangeType, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};

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
fn watched() -> [UIProperty; 8] {
    WatchedProperty::ALL.map(uia_property)
}

/// How long to wait for the pump to establish both registrations. Registration cost scales with
/// tree size — measured 38ms + 17ms on a 1500-node window — and it is the *caller's* wait budget
/// being spent, so this is bounded generously but never unbounded: failing to subscribe must cost
/// a poll, not a hang.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the pump sleeps before re-checking whether its signal still exists. Short enough that
/// a dropped signal frees the thread and its registrations promptly, long enough not to spin.
const SHUTDOWN_CHECK: Duration = Duration::from_millis(250);

/// Both handlers report the same thing — that *something* changed — because that is all the wait
/// needs; it re-reads the tree itself.
///
/// A `UIElement` must never be sent through this channel. It is apartment-affine and these
/// handlers run on UIA's RPC threads, not the pump's. Reporting *what* changed is the obvious
/// improvement and the one that would break it.
struct Notify(SyncSender<()>);

impl CustomStructureChangedEventHandler for Notify {
    fn handle(
        &self,
        _sender: &UIElement,
        _change_type: StructureChangeType,
        _runtime_id: Option<&[i32]>,
    ) -> uiautomation::Result<()> {
        // A full channel already carries "something changed", so dropping this one loses nothing.
        let _ = self.0.try_send(());
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
        let _ = self.0.try_send(());
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
///
/// Not yet called: `WindowsA11y::subscribe_changes` wires this in as a follow-up, so nothing in
/// the crate reaches this (or the rest of the module's production path) until then.
#[allow(dead_code)]
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
    // Initializes COM (MTA) on this thread; the registrations below belong to it.
    let Ok(automation) = UIAutomation::new() else {
        let _ = ready.send(false);
        return;
    };
    let Ok(window) = crate::reader::find_app_window(&automation, ctx) else {
        let _ = ready.send(false);
        return;
    };

    let structure: UIStructureChangeEventHandler = Notify(tx.clone()).into();
    if automation
        .add_structure_changed_event_handler(&window, TreeScope::Subtree, None, &structure)
        .is_err()
    {
        let _ = ready.send(false);
        return;
    }

    let property: UIPropertyChangedEventHandler = Notify(tx).into();
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

    while running.load(Ordering::Relaxed) {
        std::thread::sleep(SHUTDOWN_CHECK);
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
}
