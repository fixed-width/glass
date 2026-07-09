//! Synchronous wrapper over idb's async gRPC `CompanionService`. glass-core's
//! `Platform`/`Accessibility` seams are synchronous, so this owns a private
//! current-thread tokio runtime (like glass-android's sync `conn.rs` over a
//! socket) and blocks on each RPC. Transport is a Unix-domain socket that
//! `idb_companion --grpc-domain-sock` listens on.
use std::path::{Path, PathBuf};

use glass_core::{GlassError, Result};
use tokio::runtime::Runtime;
use tonic::transport::{Channel, Endpoint, Uri};

use super::proto;
use proto::companion_service_client::CompanionServiceClient;

/// `Debug` so callers can `?`/`unwrap` a `Result<IdbClient>`; both fields are
/// `Debug` (`Runtime`, and the tonic client over a `Channel`).
#[derive(Debug)]
pub struct IdbClient {
    rt: Runtime,
    client: CompanionServiceClient<Channel>,
}

// The input and accessibility backend code that drives these RPCs lands in later
// increments; until then the methods have no in-crate caller (the `idb` module is
// crate-private, so `pub` alone does not exempt them from `dead_code`).
#[allow(dead_code)]
impl IdbClient {
    /// Connect to `idb_companion`'s gRPC over the Unix socket at `sock`.
    pub fn connect(sock: &Path) -> Result<IdbClient> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| GlassError::Backend(format!("idb: tokio runtime: {e}")))?;
        let path: PathBuf = sock.to_path_buf();
        // The URI is ignored by the custom connector; the connector dials the UDS.
        let channel = rt
            .block_on(async move {
                Endpoint::try_from("http://[::]:50051")?
                    .connect_with_connector(tower::service_fn(move |_: Uri| {
                        let path = path.clone();
                        async move {
                            let stream = tokio::net::UnixStream::connect(&path).await?;
                            Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                        }
                    }))
                    .await
            })
            .map_err(|e| GlassError::Backend(format!("idb: connect {}: {e}", sock.display())))?;
        Ok(IdbClient {
            rt,
            client: CompanionServiceClient::new(channel),
        })
    }

    /// `accessibility_info`: `point=None` describes the whole screen (describe-all);
    /// `nested=true` requests the hierarchical format. Returns the response `json`.
    pub fn accessibility_info(&self, point: Option<(f64, f64)>, nested: bool) -> Result<String> {
        let req = proto::AccessibilityInfoRequest {
            point: point.map(|(x, y)| proto::Point { x, y }),
            format: if nested {
                proto::accessibility_info_request::Format::Nested as i32
            } else {
                proto::accessibility_info_request::Format::Legacy as i32
            },
        };
        let mut client = self.client.clone();
        let resp = self
            .rt
            .block_on(async move { client.accessibility_info(req).await })
            .map_err(|e| GlassError::Backend(format!("idb accessibility_info: {e}")))?;
        Ok(resp.into_inner().json)
    }

    /// Send one HID event stream (client-streaming `hid`). A tap is two events
    /// (touch DOWN, touch UP); a chord is modifier + key down/up pairs.
    pub fn hid(&self, events: Vec<proto::HidEvent>) -> Result<()> {
        let mut client = self.client.clone();
        self.rt
            .block_on(async move {
                let stream = tokio_stream::iter(events);
                client.hid(stream).await
            })
            .map_err(|e| GlassError::Backend(format!("idb hid: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_to_missing_socket_errors_cleanly() {
        // A UDS path that does not exist -> a structured Backend error, no panic. Runs on Linux.
        let err = IdbClient::connect(std::path::Path::new("/nonexistent/idb.sock")).unwrap_err();
        assert!(matches!(err, glass_core::GlassError::Backend(_)), "{err:?}");
    }
}
