//! Single live MCP session with last-client-wins takeover.
//!
//! Wraps rmcp's [`LocalSessionManager`] so at most one MCP session is live at a
//! time, but a *new* client always wins: `create_session` evicts whatever
//! session currently holds the slot and admits the newcomer. This is what makes
//! reconnect work under `serve --http`. A streamable-HTTP session is decoupled
//! from its TCP connection by design (so it can survive a drop and resume), and
//! (as of rmcp 1.7.0) rmcp only tears it down on an explicit `DELETE` or after
//! its `keep_alive` idle timeout (default 5 min). An agent that dies or restarts almost never
//! sends `DELETE`, so its session lingers as a zombie — and a plain admission
//! gate would reject the agent's own reconnect until that zombie expired.
//! Takeover displaces the stale session instead. glass is a single-user dev
//! tool, so favouring the newcomer is the right trade.
//!
//! Only a create that is *genuinely in flight* (`Reserving`) rejects a
//! concurrent create: there is no known id to evict yet, and two initializes
//! racing in the same instant is not a reconnect.
//!
//! The slot is released in `close_session`, but ONLY for the session id that
//! currently holds it. rmcp's `handle_delete` forwards the client's
//! `Mcp-Session-Id` header without validating it (and
//! `LocalSessionManager::close_session` returns `Ok` for an unknown id), and
//! `spawn_session_worker` calls `close_session` on every worker end — including
//! an evicted session's late one. Matching the id ensures a stale/bogus close
//! can't clear a *live* session's slot; if it could, single-session tracking
//! would be lost and a later client would run alongside the live one instead of
//! taking it over.

use std::io;
use std::sync::Mutex;

use futures::Stream;
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError, SessionError,
};
use rmcp::transport::streamable_http_server::session::{SessionId, SessionManager};
// `ServerSseMessage` is re-exported from the `session` module (it originates in
// `transport::common::server_side_http`), not from `tower`.
use rmcp::transport::streamable_http_server::session::ServerSseMessage;

/// The single admission slot.
#[derive(Debug, Default)]
enum Slot {
    /// No session — a `create_session` is admitted.
    #[default]
    Empty,
    /// A `create_session` has claimed the slot but its id isn't known yet — held
    /// from claiming the slot, through the eviction close of any superseded
    /// session, until the inner `create_session().await` resolves to `Active`.
    /// A concurrent create is rejected; a close (which carries some other id)
    /// can't match. A `create_session` whose future is dropped mid-flight would
    /// strand this state, so the reservation is held under a revert guard (see
    /// `create_session`).
    Reserving,
    /// Held by the admitted session; only a close for this id releases it.
    Active(SessionId),
}

/// Wraps [`LocalSessionManager`], keeping at most one live session with
/// last-client-wins takeover: a new `create_session` evicts the current session
/// and admits the newcomer. Only a create racing another still-in-flight create
/// is rejected.
#[derive(Debug, Default)]
pub struct SingleSessionManager {
    inner: LocalSessionManager,
    // Plain `Mutex` (not `Arc`): the manager is owned by the HTTP service (the
    // caller wraps it in an `Arc`); it is never cloned, so no internal sharing is
    // needed. The lock is only ever held for synchronous state transitions, never
    // across an `.await`.
    slot: Mutex<Slot>,
}

/// The error returned when a create races another create that is still in
/// flight (the `Reserving` window). NOT used for reconnect: a stale session is
/// evicted and the newcomer admitted, never rejected.
///
/// `LocalSessionManagerError` has no free-form / "already exists" variant, so we
/// carry the message through its `SessionError(SessionError::Io(_))` path, which
/// renders as a clear human-readable string.
fn busy_error() -> LocalSessionManagerError {
    LocalSessionManagerError::SessionError(SessionError::Io(io::Error::other(
        "glass: another client is initializing a session; retry in a moment",
    )))
}

/// Reverts a `Reserving` reservation to `Empty` if the `create_session` future
/// is dropped (task cancellation or a panic) before it commits to `Active`/
/// `Empty`. Without this, a client that disconnects mid-handshake — rmcp awaits
/// `create_session` inline in the request task, so hyper drops the future at its
/// suspended await — would strand the slot at `Reserving`, and every subsequent
/// create would `busy_error()` forever (a permanent lockout, the exact failure
/// this gate exists to prevent). Disarmed once the create commits.
struct ReservationGuard<'a> {
    slot: &'a Mutex<Slot>,
    armed: bool,
}

impl ReservationGuard<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Recover a poisoned lock rather than double-panic: reverting the stranded
        // reservation matters more than propagating an unrelated prior panic. Only
        // clear the slot if it's still our `Reserving` — a committed `Active`/
        // `Empty` (or another create's reservation) must never be clobbered.
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*slot, Slot::Reserving) {
            *slot = Slot::Empty;
        }
    }
}

impl SessionManager for SingleSessionManager {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        // Claim the slot before the await. Take over an existing session (the
        // common reconnect case: a client died leaving a lingering session);
        // reject only a create that is itself still in flight, since there's no
        // known id to evict yet.
        let evict = {
            let mut slot = self.slot.lock().expect("session slot mutex");
            match &*slot {
                Slot::Reserving => return Err(busy_error()),
                Slot::Active(old) => {
                    let old = old.clone();
                    *slot = Slot::Reserving;
                    Some(old)
                }
                Slot::Empty => {
                    *slot = Slot::Reserving;
                    None
                }
            }
        };
        // From here until we commit, a dropped future must not strand `Reserving`.
        let mut guard = ReservationGuard {
            slot: &self.slot,
            armed: true,
        };
        // Best-effort eviction of the superseded session, never holding the lock
        // across the await. The result is ignored: the goal is to admit the
        // newcomer, and a stale worker exits on its own `keep_alive` timeout even
        // if this close races it. The id guard in `close_session` keeps that
        // session's own late worker-end close from clearing the new slot below.
        if let Some(old) = evict {
            let _ = self.inner.close_session(&old).await;
        }
        match self.inner.create_session().await {
            Ok((id, transport)) => {
                *self.slot.lock().expect("session slot mutex") = Slot::Active(id.clone());
                guard.disarm();
                Ok((id, transport))
            }
            Err(e) => {
                // Release the slot if the inner manager failed to create.
                *self.slot.lock().expect("session slot mutex") = Slot::Empty;
                guard.disarm();
                Err(e)
            }
        }
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        let r = self.inner.close_session(id).await;
        // Release the slot ONLY for the admitted session. A close for a
        // stale/bogus id (an unvalidated DELETE header, or a superseded session's
        // late worker-end) must not free a live session's slot — see the module
        // docs.
        let mut slot = self.slot.lock().expect("session slot mutex");
        if matches!(&*slot, Slot::Active(active) if active == id) {
            *slot = Slot::Empty;
        }
        r
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.create_stream(id, message).await
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        self.inner.accept_message(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.resume(id, last_event_id).await
    }
}

#[cfg(test)]
impl SingleSessionManager {
    /// Test-only view of which session (if any) currently holds the slot.
    /// `None` covers both `Empty` and the transient `Reserving`.
    fn active_id(&self) -> Option<SessionId> {
        match &*self.slot.lock().expect("session slot mutex") {
            Slot::Active(id) => Some(id.clone()),
            Slot::Empty | Slot::Reserving => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reconnect_takes_over_then_close_releases() {
        let m = SingleSessionManager::default();
        let (id1, _t1) = m.create_session().await.expect("first session admitted");
        assert_eq!(m.active_id().as_ref(), Some(&id1));

        // A new client (the reconnect case) takes over the live slot instead of
        // being rejected. It gets a fresh session; the slot now tracks it.
        let (id2, _t2) = m
            .create_session()
            .await
            .expect("reconnect takes over the slot");
        assert_ne!(id1, id2, "takeover admits a fresh session");
        assert_eq!(m.active_id().as_ref(), Some(&id2));

        // The evicted session's own late worker-end close must NOT clear the new
        // slot (id guard) — otherwise the next client would coexist, not take over.
        m.close_session(&id1).await.expect("stale close returns Ok");
        assert_eq!(
            m.active_id().as_ref(),
            Some(&id2),
            "a close for the evicted id must not clear the live slot"
        );

        // The active session's own close releases the slot to Empty.
        m.close_session(&id2)
            .await
            .expect("close releases the slot");
        assert_eq!(m.active_id(), None);
        let (_id3, _t3) = m.create_session().await.expect("slot reusable after close");
    }

    #[tokio::test]
    async fn close_with_foreign_id_does_not_release_the_slot() {
        use std::sync::Arc;
        let m = SingleSessionManager::default();
        let (id, _t) = m.create_session().await.expect("first session admitted");
        // rmcp's handle_delete forwards the client's Mcp-Session-Id header without
        // validating it, and LocalSessionManager::close_session returns Ok for an
        // unknown id — so a DELETE carrying a bogus/stale id (or an evicted
        // session's late worker-end) must NOT clear the live slot. If it did,
        // single-session tracking would be lost and the next create would admit a
        // second live session instead of taking over the first.
        let bogus: SessionId = Arc::from("not-the-active-session");
        let _ = m.close_session(&bogus).await;
        assert_eq!(
            m.active_id().as_ref(),
            Some(&id),
            "slot must still track the live session after a foreign-id close"
        );
        // The admitted session's own close still releases it.
        m.close_session(&id)
            .await
            .expect("closing the active session releases");
        assert_eq!(m.active_id(), None);
    }

    #[tokio::test]
    async fn takeover_evicts_old_session_from_inner_manager() {
        // Takeover must actually evict the superseded session from the inner
        // manager, not merely swap the slot — otherwise the stale session lingers
        // (routable, until its 5-min keep_alive) and this fix's purpose is lost.
        let m = SingleSessionManager::default();
        let (id1, _t1) = m.create_session().await.expect("first session admitted");
        assert!(
            m.has_session(&id1).await.expect("has_session"),
            "id1 live before takeover"
        );

        let (id2, _t2) = m.create_session().await.expect("takeover admits newcomer");
        assert!(
            !m.has_session(&id1).await.expect("has_session"),
            "the superseded session must be evicted from the inner manager"
        );
        assert!(
            m.has_session(&id2).await.expect("has_session"),
            "newcomer is live"
        );
    }

    #[tokio::test]
    async fn create_while_reserving_is_rejected() {
        // The one remaining rejection path: a create arriving while another is
        // genuinely in flight (`Reserving`) has no known id to evict, so it's
        // rejected rather than admitted — two concurrent initializes must not both
        // proceed. A real await-interleaving race isn't deterministic, so drive the
        // state directly (the `Slot` machine is the contract under test).
        let m = SingleSessionManager::default();
        *m.slot.lock().expect("session slot mutex") = Slot::Reserving;
        assert!(
            m.create_session().await.is_err(),
            "a create must be rejected while another is in flight"
        );
        assert!(
            matches!(*m.slot.lock().expect("session slot mutex"), Slot::Reserving),
            "the rejected create must not clobber the in-flight reservation"
        );
    }

    #[test]
    fn reservation_guard_reverts_stranded_reserving_on_drop() {
        // A `create_session` future dropped mid-flight (client vanished during the
        // handshake) must self-heal the slot instead of wedging it at `Reserving`.
        let m = SingleSessionManager::default();
        *m.slot.lock().expect("session slot mutex") = Slot::Reserving;
        drop(ReservationGuard {
            slot: &m.slot,
            armed: true,
        });
        assert!(
            matches!(*m.slot.lock().expect("session slot mutex"), Slot::Empty),
            "a dropped armed guard must revert a stranded reservation to Empty"
        );
    }

    #[test]
    fn reservation_guard_disarmed_is_a_noop() {
        // Once the create commits, dropping the guard must leave the slot alone.
        let m = SingleSessionManager::default();
        *m.slot.lock().expect("session slot mutex") = Slot::Reserving;
        let mut g = ReservationGuard {
            slot: &m.slot,
            armed: true,
        };
        g.disarm();
        drop(g);
        assert!(
            matches!(*m.slot.lock().expect("session slot mutex"), Slot::Reserving),
            "a disarmed guard must not touch the slot"
        );
    }

    #[tokio::test]
    async fn reservation_guard_leaves_a_committed_session_untouched() {
        // If the slot has already advanced to `Active` by the time an armed guard
        // drops (e.g. another path committed), the guard must not clobber it.
        let m = SingleSessionManager::default();
        let (id, _t) = m.create_session().await.expect("session admitted");
        drop(ReservationGuard {
            slot: &m.slot,
            armed: true,
        });
        assert_eq!(
            m.active_id().as_ref(),
            Some(&id),
            "an armed guard must only revert `Reserving`, never a live `Active`"
        );
    }
}
