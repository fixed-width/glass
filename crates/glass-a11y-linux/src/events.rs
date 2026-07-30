//! AT-SPI change notifications, so a wait stops re-walking a tree that has not changed.
//!
//! The stream this subscribes to is **session-wide**: every accessible application on the bus
//! emits onto it, each event carrying its emitter's unique bus name. Probed on 2026-07-30, a
//! subscription taken while driving one app received events from unrelated desktop applications.
//! So the filter below is not an optimization — without it a wait would wake on every unrelated
//! desktop change, which is worse than the polling it replaces.

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use glass_core::{AxContext, ChangeSignal, ChangeWait};

/// Whether an event from `emitter` concerns the app glass is driving — a unique-name match
/// (`:1.15`), and the only part of this file testable without a bus.
pub(crate) fn concerns_app(app_bus_name: &str, emitter_bus_name: &str) -> bool {
    !app_bus_name.is_empty() && app_bus_name == emitter_bus_name
}

/// A [`ChangeSignal`] fed by an AT-SPI event stream running on its own thread.
pub(crate) struct AtspiChanges {
    rx: Receiver<()>,
    live: bool,
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
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            let _ = ready_tx.send(false);
            return;
        };
        rt.block_on(pump(&ctx, tx, ready_tx));
    });
    // Wait for the subscription to be established before returning: a caller reads the tree the
    // moment it gets a signal back, and a change landing before the match rule is in place would
    // be lost.
    match ready_rx.recv_timeout(SUBSCRIBE_TIMEOUT) {
        Ok(true) => Some(Box::new(AtspiChanges { rx, live: true })),
        _ => None,
    }
}

/// How long to wait for the registry to accept the subscription — bounded, because failing to
/// subscribe must cost a poll, not a hang.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Register for object events and forward the ones this app emitted.
async fn pump(ctx: &AxContext, tx: SyncSender<()>, ready: std::sync::mpsc::Sender<bool>) {
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
    let app_bus_name = match crate::reader::app_bus_name(ctx, &conn).await {
        Some(name) => name,
        None => {
            let _ = ready.send(false);
            return;
        }
    };
    let _ = ready.send(true);

    let stream = conn.event_stream();
    let mut stream = std::pin::pin!(stream);
    loop {
        let next = std::future::poll_fn(|cx| {
            zbus::export::futures_core::Stream::poll_next(stream.as_mut(), cx)
        })
        .await;
        match next {
            // Only this app's events. `tx` full means an unread change is already queued, which
            // says the same thing, so dropping this one loses nothing.
            Some(Ok(ev)) => {
                if emitter_bus_name(&ev).is_some_and(|n| concerns_app(&app_bus_name, &n)) {
                    let _ = tx.try_send(());
                }
            }
            // A transport error is not the end of the stream; keep listening.
            Some(Err(_)) => {}
            // The stream ended: dropping `tx` disconnects the receiver, which is how the signal
            // learns to report `Unusable` rather than looking quiet forever.
            None => return,
        }
    }
}

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
        assert!(concerns_app(":1.15", ":1.15"));
        assert!(!concerns_app(":1.15", ":1.42"));
    }

    #[test]
    fn an_unknown_app_name_matches_nothing() {
        // Failing open here would make every desktop event look like ours.
        assert!(!concerns_app("", ""));
        assert!(!concerns_app("", ":1.15"));
    }
}
