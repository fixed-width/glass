//! AT-SPI change notifications, so a wait stops re-walking a tree that has not changed.
//!
//! The stream carries every app registered on the bus it is taken from, not just the one being
//! driven — each event names its emitter's unique bus name. So the events are filtered, and
//! filtered to a *set* of names: a walk crosses process boundaries (each child is proxied at its
//! own name), so an app embedding an out-of-process view publishes part of its tree from a second
//! connection, and matching one name would walk that subtree while dropping its events.
//!
//! (A probe on 2026-07-30 watched a *host* session bus and saw unrelated desktop applications on
//! it. glass reads its own private bus, where that does not apply — the multi-process case above
//! is what the filter is really for.)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use glass_core::{AxContext, ChangeSignal, ChangeWait};

/// Whether an event from `emitter` concerns the app glass is driving — a unique-name match
/// (`:1.15`) against every connection the app publishes from.
pub(crate) fn concerns_app(app_bus_names: &[String], emitter_bus_name: &str) -> bool {
    app_bus_names.iter().any(|n| n == emitter_bus_name)
}

/// A [`ChangeSignal`] fed by an AT-SPI event stream running on its own thread.
pub(crate) struct AtspiChanges {
    rx: Receiver<()>,
    live: bool,
    /// Cleared on drop to stop the pump. Without it the pump owns its own connection, so its
    /// stream never ends and the thread — with a tokio runtime, a bus connection and a registry
    /// registration that makes every app on the desktop forward object events — would outlive
    /// every wait that created one.
    running: Arc<AtomicBool>,
}

impl Drop for AtspiChanges {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl ChangeSignal for AtspiChanges {
    fn wait(&mut self, timeout: Duration) -> ChangeWait {
        if !self.live {
            return ChangeWait::Unusable;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(()) => {
                // Drain whatever else arrived: a burst of events is one reason to re-read, not
                // one re-read each.
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

/// Subscribe to changes for the app in `ctx`, or `None` if no subscription could be established —
/// the caller then polls exactly as it did before.
pub(crate) fn subscribe(ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
    let ctx = ctx.clone();
    // Capacity 1: the receiver only needs to learn that *something* changed, and a full channel
    // already carries that fact, so a chatty app cannot grow an unbounded backlog here.
    let (tx, rx) = sync_channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let pump_running = running.clone();
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            let _ = ready_tx.send(false);
            return;
        };
        rt.block_on(pump(&ctx, tx, ready_tx, pump_running));
    });
    // Wait for the subscription to be established before returning: a caller reads the tree the
    // moment it gets a signal back, and a change landing before the match rule is in place would
    // be lost.
    match ready_rx.recv_timeout(SUBSCRIBE_TIMEOUT) {
        Ok(true) => Some(Box::new(AtspiChanges {
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

/// How long to wait for the registry to accept the subscription: a registry round-trip plus one
/// `GetConnectionUnixProcessID` per registered app, so generous — but bounded, because failing to
/// subscribe must cost a poll, not a hang.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Register for object events and forward the ones this app emitted.
async fn pump(
    ctx: &AxContext,
    tx: SyncSender<()>,
    ready: std::sync::mpsc::Sender<bool>,
    running: Arc<AtomicBool>,
) {
    let Some(addr) = ctx.a11y_bus_addr.as_deref() else {
        let _ = ready.send(false);
        return;
    };
    let Ok(parsed) = addr.try_into() else {
        let _ = ready.send(false);
        return;
    };
    let Ok(conn) = AccessibilityConnection::from_address(parsed).await else {
        let _ = ready.send(false);
        return;
    };
    if conn
        .register_event::<atspi_common::events::ObjectEvents>()
        .await
        .is_err()
    {
        let _ = ready.send(false);
        return;
    }
    let app_bus_names = crate::reader::app_bus_names(ctx, &conn).await;
    if app_bus_names.is_empty() {
        let _ = ready.send(false);
        return;
    }

    // The stream is attached *before* the handshake, not after: until `event_stream` runs there is
    // no active receiver on the connection, and zbus drops messages that arrive with none. The
    // match rule alone does not hold them — which is the race the handshake exists to close.
    let stream = conn.event_stream();
    let mut stream = std::pin::pin!(stream);
    let _ = ready.send(true);
    let mut consecutive_errors = 0u32;
    while running.load(Ordering::Relaxed) {
        // Bounded, so a dropped signal is noticed on a quiet app too: the stream itself never ends
        // while this task owns the connection, so without a timeout the thread would park in
        // `poll_next` forever waiting for an event that may never come.
        let next = tokio::time::timeout(
            SHUTDOWN_CHECK,
            std::future::poll_fn(|cx| {
                zbus::export::futures_core::Stream::poll_next(stream.as_mut(), cx)
            }),
        )
        .await;
        match next {
            // Only this app's events. `tx` full means an unread change is already queued, which
            // says the same thing, so dropping this one loses nothing.
            Ok(Some(Ok(ev))) => {
                consecutive_errors = 0;
                if emitter_bus_name(&ev).is_some_and(|n| concerns_app(&app_bus_names, &n))
                    && tx
                        .try_send(())
                        .is_err_and(|e| matches!(e, std::sync::mpsc::TrySendError::Disconnected(_)))
                {
                    return; // the wait that subscribed is over
                }
            }
            // Whether a decode failure ends the stream is not established here, so neither
            // assumption is made: one is tolerated, a run of them is treated as a stream that is
            // not coming back rather than spun on at full tilt.
            Ok(Some(Err(_))) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return;
                }
            }
            // The stream ended: dropping `tx` disconnects the receiver, which is how the signal
            // learns to report `Unusable` rather than looking quiet forever.
            Ok(None) => return,
            // No event within the shutdown check — loop and re-test `running`.
            Err(_) => {}
        }
    }
    // Leaving the registration behind would keep every app on the desktop forwarding object
    // events to a bus nobody reads.
    let _ = conn
        .deregister_event::<atspi_common::events::ObjectEvents>()
        .await;
}

/// How long the pump waits for an event before re-checking whether its signal still exists. Short
/// enough that a dropped signal frees the thread promptly, long enough not to spin.
const SHUTDOWN_CHECK: Duration = Duration::from_millis(250);

/// Consecutive stream errors before the pump gives up. One is a decode failure; a run of them is a
/// transport that is not coming back, and retrying it forever would burn a core.
const MAX_CONSECUTIVE_ERRORS: u32 = 20;

/// The unique bus name that emitted `ev`. Object events are the only kind subscribed to, so any
/// other kind is not this app's business.
fn emitter_bus_name(ev: &atspi::Event) -> Option<String> {
    use atspi_common::EventProperties;
    match ev {
        atspi::Event::Object(o) => Some(o.sender().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_apps_events_count() {
        // The stream carries the whole session: unrelated desktop applications emit onto it, so an
        // unfiltered wait would wake on a clock tick in another window.
        let app = vec![":1.15".to_string()];
        assert!(concerns_app(&app, ":1.15"));
        assert!(!concerns_app(&app, ":1.42"));
    }

    #[test]
    fn a_second_process_of_the_same_app_counts_too() {
        // A walk crosses process boundaries — an embedded view publishes its subtree from its own
        // connection — so filtering on one name would walk that subtree and drop its events.
        let app = vec![":1.15".to_string(), ":1.42".to_string()];
        assert!(concerns_app(&app, ":1.42"));
        assert!(!concerns_app(&app, ":1.99"));
    }

    #[test]
    fn an_unresolved_app_matches_nothing() {
        // Failing open here would make every desktop event look like ours.
        assert!(!concerns_app(&[], ":1.15"));
    }
}
