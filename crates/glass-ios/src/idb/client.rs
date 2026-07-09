//! Synchronous wrapper over idb's async gRPC `CompanionService`. glass-core's
//! `Platform`/`Accessibility` seams are synchronous, so this owns a private
//! current-thread tokio runtime and blocks on each RPC. Transport is a
//! Unix-domain socket that `idb_companion --grpc-domain-sock` listens on.
//!
//! See [`IdbClient`] for the threading invariant that `block_on` imposes, and
//! [`CONNECT_TIMEOUT`]/[`RPC_TIMEOUT`] for the deadlines that keep a wedged
//! companion from hanging the caller.
use std::path::{Path, PathBuf};
use std::time::Duration;

use glass_core::{GlassError, Result};
use tokio::runtime::Runtime;
use tonic::transport::{Channel, Endpoint, Uri};

use super::proto;
use proto::companion_service_client::CompanionServiceClient;

/// Deadline for establishing the connection (dial + HTTP/2 handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline for a single RPC. 30s mirrors glass-android's socket timeout.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Blocking handle to `idb_companion`'s gRPC service.
///
/// **Threading invariant:** must be driven from a thread with *no* ambient tokio
/// runtime. Each method calls [`Runtime::block_on`], which panics ("Cannot start a
/// runtime from within a runtime") if a tokio runtime is already active on the
/// calling thread. glass-mcp dispatches every tool body on a dedicated, non-tokio
/// `glass-platform` thread, which satisfies this. Unlike `glass-a11y-linux`'s
/// AT-SPI reader (a fresh thread + current-thread runtime per call), the persistent
/// tonic `Channel` here is bound to `rt` and owned for the client's lifetime, so it
/// cannot be re-created per call — hence the invariant is documented rather than
/// enforced by re-threading.
///
/// `Debug` so callers can `?`/`unwrap` a `Result<IdbClient>`; both fields are
/// `Debug` (`Runtime`, and the tonic client over a `Channel`).
#[derive(Debug)]
pub struct IdbClient {
    rt: Runtime,
    client: CompanionServiceClient<Channel>,
}

/// Fold a `timeout(op).await` outcome into a `Result`: the inner error becomes a
/// `Backend` carrying it, an elapsed deadline becomes a `Backend` timeout error.
/// Generic over the inner error so it serves both `connect` (`transport::Error`)
/// and the RPCs (`tonic::Status`).
fn map_timed<T, E: std::fmt::Display>(
    op: &str,
    deadline: Duration,
    outcome: std::result::Result<std::result::Result<T, E>, tokio::time::error::Elapsed>,
) -> Result<T> {
    match outcome {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(GlassError::Backend(format!("{op}: {e}"))),
        Err(_elapsed) => Err(GlassError::Backend(format!(
            "{op} timed out after {}s",
            deadline.as_secs()
        ))),
    }
}

impl IdbClient {
    /// Connect to `idb_companion`'s gRPC over the Unix socket at `sock`.
    pub fn connect(sock: &Path) -> Result<IdbClient> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| GlassError::Backend(format!("idb: tokio runtime: {e}")))?;
        let path: PathBuf = sock.to_path_buf();
        // The URI is ignored by the custom connector; the connector dials the UDS.
        let outcome = rt.block_on(async move {
            tokio::time::timeout(CONNECT_TIMEOUT, async move {
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
            .await
        });
        let channel = map_timed(
            &format!("idb: connect {}", sock.display()),
            CONNECT_TIMEOUT,
            outcome,
        )?;
        Ok(IdbClient {
            rt,
            client: CompanionServiceClient::new(channel),
        })
    }

    /// A client whose channel is never dialed (lazy), for `IosPlatform` unit tests that
    /// build a platform without a live companion. Any RPC on it would fail; these tests
    /// exercise only the platform's state machine and never issue one.
    #[cfg(test)]
    pub(crate) fn for_test() -> IdbClient {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a current-thread runtime for the stub test client");
        // `connect_lazy` never dials, but it wires up the connector inside a runtime
        // context, so build the channel while `rt` is entered.
        let channel = {
            let _guard = rt.enter();
            Endpoint::from_static("http://127.0.0.1:0").connect_lazy()
        };
        IdbClient {
            rt,
            client: CompanionServiceClient::new(channel),
        }
    }

    /// Describe the whole accessibility tree: idb's `accessibility_info` over the entire
    /// screen (no point) in the nested/hierarchical format. Returns the response `json`.
    pub fn describe_all(&self) -> Result<String> {
        self.ensure_off_runtime("idb accessibility_info")?;
        let req = proto::AccessibilityInfoRequest {
            point: None,
            format: proto::accessibility_info_request::Format::Nested as i32,
        };
        let mut client = self.client.clone();
        let outcome = self.rt.block_on(async move {
            tokio::time::timeout(RPC_TIMEOUT, client.accessibility_info(req)).await
        });
        let resp = map_timed("idb accessibility_info", RPC_TIMEOUT, outcome)?;
        Ok(resp.into_inner().json)
    }

    /// Send one HID event stream (client-streaming `hid`). A tap is two events
    /// (touch DOWN, touch UP); a chord is modifier + key down/up pairs.
    pub fn hid(&self, events: Vec<proto::HidEvent>) -> Result<()> {
        self.ensure_off_runtime("idb hid")?;
        let mut client = self.client.clone();
        let outcome = self.rt.block_on(async move {
            let stream = tokio_stream::iter(events);
            tokio::time::timeout(RPC_TIMEOUT, client.hid(stream)).await
        });
        map_timed("idb hid", RPC_TIMEOUT, outcome)?;
        Ok(())
    }

    /// Guard the `block_on` threading invariant (see the type doc): calling an RPC from a
    /// thread that already has an active tokio runtime would panic inside
    /// [`Runtime::block_on`]. Turn that future wiring mistake into a structured error so it
    /// surfaces as a clean failure rather than a panic.
    fn ensure_off_runtime(&self, op: &str) -> Result<()> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(GlassError::Backend(format!(
                "{op}: idb client called from within an async runtime \
                 (it must run on a thread with no ambient tokio runtime)"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_timed_folds_an_elapsed_deadline_into_a_backend_timeout() {
        // Produce a real `tokio::time::error::Elapsed` from a timeout that fires over a
        // never-ready future, then assert `map_timed` maps it to a Backend timeout error
        // (the branch the transport-error tests below don't cover).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a current-thread runtime");
        let outcome: std::result::Result<std::result::Result<(), String>, _> = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(1),
                std::future::pending::<std::result::Result<(), String>>(),
            )
            .await
        });
        let err = map_timed("idb hid", Duration::from_secs(30), outcome).unwrap_err();
        assert!(
            matches!(err, GlassError::Backend(ref msg) if msg.contains("timed out after 30s")),
            "{err:?}"
        );
    }

    #[test]
    fn connect_to_missing_socket_errors_cleanly() {
        // A UDS path that does not exist -> a structured Backend error, no panic. Runs on Linux.
        let err = IdbClient::connect(std::path::Path::new("/nonexistent/idb.sock")).unwrap_err();
        assert!(matches!(err, glass_core::GlassError::Backend(_)), "{err:?}");
    }

    #[test]
    fn connect_to_dead_socket_errors_cleanly() {
        // A UDS path that exists but has no listener behind it — the "stale idb
        // socket" case (companion died, leaving its socket file). `connect` is
        // refused at the transport layer (ECONNREFUSED) and maps to a structured
        // Backend error, no panic, promptly.
        //
        // Deterministic by construction: bind then immediately drop the listener,
        // so the socket file lingers with nothing listening. There is no live peer,
        // hence no accept-vs-handshake timing race (an earlier version raced on
        // whether tonic's h2 preface write buffered before the peer's reset).
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("idb.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        }
        let err = IdbClient::connect(&sock).unwrap_err();
        assert!(matches!(err, glass_core::GlassError::Backend(_)), "{err:?}");
    }
}
