//! Single-client admission gate: at most one live MCP session at a time.
//!
//! Wraps rmcp's [`LocalSessionManager`] and enforces that only one MCP session
//! is alive at a time. `create_session` claims a single slot (rejecting a second
//! client while one is live); `close_session` releases it. Every other
//! [`SessionManager`] method is pure delegation to the inner manager.
//!
//! The slot is released in `close_session`, but ONLY for the admitted session's
//! id. rmcp's `handle_delete` forwards the client's `Mcp-Session-Id` header
//! without validating it (and `LocalSessionManager::close_session` returns `Ok`
//! for an unknown id), and `spawn_session_worker` calls `close_session` on every
//! worker end — including a superseded session's. Matching the id ensures a
//! stale/bogus close can't free a live session's slot (which would let a second
//! client in alongside the first).

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
    /// A `create_session` has claimed the slot but its id isn't known yet (the
    /// inner `create_session().await` is in flight). A concurrent create is
    /// rejected; a close (which carries some other id) can't match.
    Reserving,
    /// Held by the admitted session; only a close for this id releases it.
    Active(SessionId),
}

/// Wraps [`LocalSessionManager`], enforcing at most one concurrent session.
///
/// A second `create_session` while one is live returns an error (surfaced to the
/// second client as an error response — "another session is active").
#[derive(Debug, Default)]
pub struct SingleSessionManager {
    inner: LocalSessionManager,
    // Plain `Mutex` (not `Arc`): the manager is owned by the HTTP service (the
    // caller wraps it in an `Arc`); it is never cloned, so no internal sharing is
    // needed. The lock is only ever held for synchronous state transitions, never
    // across an `.await`.
    slot: Mutex<Slot>,
}

/// The error returned when a second client tries to attach while one is live.
///
/// `LocalSessionManagerError` has no free-form / "already exists" variant, so we
/// carry the message through its `SessionError(SessionError::Io(_))` path, which
/// renders as a clear human-readable string.
fn busy_error() -> LocalSessionManagerError {
    LocalSessionManagerError::SessionError(SessionError::Io(io::Error::other(
        "glass serves one client at a time; another session is active",
    )))
}

impl SessionManager for SingleSessionManager {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        // Claim the single slot before the await; reject if already taken.
        {
            let mut slot = self.slot.lock().unwrap();
            if !matches!(*slot, Slot::Empty) {
                return Err(busy_error());
            }
            *slot = Slot::Reserving;
        }
        match self.inner.create_session().await {
            Ok((id, transport)) => {
                *self.slot.lock().unwrap() = Slot::Active(id.clone());
                Ok((id, transport))
            }
            Err(e) => {
                // Release the slot if the inner manager failed to create.
                *self.slot.lock().unwrap() = Slot::Empty;
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
        let mut slot = self.slot.lock().unwrap();
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn admits_one_rejects_second_releases_on_close() {
        let m = SingleSessionManager::default();
        let (id, _t) = m.create_session().await.expect("first session admitted");
        assert!(
            m.create_session().await.is_err(),
            "second session must be rejected while one is live"
        );
        m.close_session(&id).await.expect("close releases the slot");
        let (_id2, _t2) = m
            .create_session()
            .await
            .expect("slot reusable after close");
    }

    #[tokio::test]
    async fn close_with_foreign_id_does_not_release_the_slot() {
        use std::sync::Arc;
        let m = SingleSessionManager::default();
        let (id, _t) = m.create_session().await.expect("first session admitted");
        // rmcp's handle_delete forwards the client's Mcp-Session-Id header without
        // validating it, and LocalSessionManager::close_session returns Ok for an
        // unknown id — so a DELETE carrying a bogus/stale id (or a superseded
        // session's late worker-end) must NOT free the live slot.
        let bogus: SessionId = Arc::from("not-the-active-session");
        let _ = m.close_session(&bogus).await;
        assert!(
            m.create_session().await.is_err(),
            "slot must stay claimed after a close for a non-active id"
        );
        // The admitted session's own close still releases it.
        m.close_session(&id).await.expect("closing the active session releases");
        let (_id2, _t2) =
            m.create_session().await.expect("slot reusable after the real close");
    }
}
