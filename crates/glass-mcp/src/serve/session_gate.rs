//! Single-client admission gate: at most one live MCP session at a time.
//!
//! Wraps rmcp's [`LocalSessionManager`] and enforces that only one MCP session
//! is alive at a time. `create_session` claims a single slot (rejecting a second
//! client while one is live); `close_session` releases it. Every other
//! [`SessionManager`] method is pure delegation to the inner manager.
//!
//! Releasing the slot in `close_session` correctly handles client disconnect:
//! rmcp's `spawn_session_worker` calls `close_session` both on an explicit HTTP
//! DELETE *and* when the per-session worker ends (i.e. the client drops), so the
//! slot is freed in both cases.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::Stream;
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError, SessionError,
};
use rmcp::transport::streamable_http_server::session::{SessionId, SessionManager};
// `ServerSseMessage` is re-exported from the `session` module (it originates in
// `transport::common::server_side_http`), not from `tower`.
use rmcp::transport::streamable_http_server::session::ServerSseMessage;

/// Wraps [`LocalSessionManager`], enforcing at most one concurrent session.
///
/// A second `create_session` while one is live returns an error (surfaced to the
/// second client as an error response — "another session is active").
#[derive(Debug, Default)]
pub struct SingleSessionManager {
    inner: LocalSessionManager,
    // Plain `AtomicBool` (not `Arc`): the manager is owned by the HTTP service (the
    // caller wraps it in an `Arc`); it is never cloned, so no internal sharing is needed.
    occupied: AtomicBool,
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
        // Claim the single slot; reject if already taken.
        if self.occupied.swap(true, Ordering::AcqRel) {
            return Err(busy_error());
        }
        match self.inner.create_session().await {
            Ok(v) => Ok(v),
            Err(e) => {
                // Release the slot if the inner manager failed to create.
                self.occupied.store(false, Ordering::Release);
                Err(e)
            }
        }
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        let r = self.inner.close_session(id).await;
        // Always release the slot: a close is final whether or not the inner
        // teardown reported an error.
        self.occupied.store(false, Ordering::Release);
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
}
