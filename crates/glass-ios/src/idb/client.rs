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

/// Deadline for one `hid` stream. A `HidSwipe`/`HidDelay`/`HidPinch` plays out over
/// its `duration` seconds on the device and holds the streaming RPC open at least
/// that long, so a flat [`RPC_TIMEOUT`] aborts any gesture longer than it mid-stream
/// (issue #116). Budget the summed event durations plus [`RPC_TIMEOUT`] of margin: a
/// long drag runs to completion, yet a wedged companion is still bounded, now relative
/// to the expected work. An instantaneous `HidPress` (tap, key chord) contributes
/// nothing, so those streams keep the flat deadline. A non-finite or non-positive
/// `duration` is dropped so a malformed event can't poison the sum.
fn hid_timeout(events: &[proto::HidEvent]) -> Duration {
    use proto::hid_event::Event;
    let device_secs: f64 = events
        .iter()
        .filter_map(|e| match e.event.as_ref()? {
            // Every variant that runs over wall-clock time carries a `duration` (seconds).
            Event::Swipe(s) => Some(s.duration),
            Event::Delay(d) => Some(d.duration),
            Event::Pinch(p) => Some(p.duration),
            Event::Press(_) => None,
        })
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .sum();
    // `try_from_secs_f64` rejects a NaN/inf/overflowing sum (→ ZERO), and
    // `saturating_add` caps the total at `Duration::MAX`; the result never panics.
    RPC_TIMEOUT.saturating_add(Duration::try_from_secs_f64(device_secs).unwrap_or(Duration::ZERO))
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
        // Derive the deadline from the gesture's own duration so a long swipe isn't
        // aborted mid-stream by a flat timeout (issue #116); see [`hid_timeout`].
        let timeout = hid_timeout(&events);
        let mut client = self.client.clone();
        let outcome = self.rt.block_on(async move {
            let stream = tokio_stream::iter(events);
            tokio::time::timeout(timeout, client.hid(stream)).await
        });
        map_timed("idb hid", timeout, outcome)?;
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

    fn swipe_event(duration: f64) -> proto::HidEvent {
        use proto::hid_event::{Event, HidSwipe};
        proto::HidEvent {
            event: Some(Event::Swipe(HidSwipe {
                start: None,
                end: None,
                delta: 0.0,
                duration,
            })),
        }
    }

    fn touch_event() -> proto::HidEvent {
        use proto::hid_event::{Event, HidPress};
        proto::HidEvent {
            event: Some(Event::Press(HidPress {
                action: None,
                direction: 0,
            })),
        }
    }

    fn delay_event(duration: f64) -> proto::HidEvent {
        use proto::hid_event::{Event, HidDelay};
        proto::HidEvent {
            event: Some(Event::Delay(HidDelay { duration })),
        }
    }

    fn pinch_event(duration: f64) -> proto::HidEvent {
        use proto::hid_event::{Event, HidPinch};
        proto::HidEvent {
            event: Some(Event::Pinch(HidPinch {
                center: None,
                scale: 1.0,
                duration,
                radius: 0.0,
            })),
        }
    }

    #[test]
    fn hid_timeout_of_a_stream_without_swipes_is_the_base_deadline() {
        // A tap or key chord carries no swipe, so it keeps the flat base deadline.
        assert_eq!(hid_timeout(&[touch_event(), touch_event()]), RPC_TIMEOUT);
    }

    #[test]
    fn hid_timeout_extends_the_base_deadline_by_a_long_swipe_duration() {
        // A 45s swipe would trip a flat 30s deadline mid-gesture (issue #116); the
        // deadline must cover the swipe plus the base margin.
        assert_eq!(
            hid_timeout(&[swipe_event(45.0)]),
            RPC_TIMEOUT + Duration::from_secs(45)
        );
    }

    #[test]
    fn hid_timeout_sums_the_durations_of_multiple_swipes() {
        assert_eq!(
            hid_timeout(&[swipe_event(10.0), swipe_event(20.0)]),
            RPC_TIMEOUT + Duration::from_secs(30)
        );
    }

    #[test]
    fn hid_timeout_extends_the_base_deadline_by_a_long_delay_duration() {
        // A `HidDelay` plays out over wall-clock time on the device just like a swipe,
        // so a long delay must extend the deadline too (else issue #116 recurs).
        assert_eq!(
            hid_timeout(&[delay_event(45.0)]),
            RPC_TIMEOUT + Duration::from_secs(45)
        );
    }

    #[test]
    fn hid_timeout_extends_the_base_deadline_by_a_long_pinch_duration() {
        // A `HidPinch` (multi-touch) also runs over its `duration`; budget it as well.
        assert_eq!(
            hid_timeout(&[pinch_event(45.0)]),
            RPC_TIMEOUT + Duration::from_secs(45)
        );
    }

    #[test]
    fn hid_timeout_ignores_a_swipe_with_a_non_finite_duration() {
        // Defensive: a NaN/inf duration must not poison the deadline arithmetic.
        assert_eq!(hid_timeout(&[swipe_event(f64::NAN)]), RPC_TIMEOUT);
        assert_eq!(hid_timeout(&[swipe_event(f64::INFINITY)]), RPC_TIMEOUT);
    }

    #[test]
    fn hid_timeout_ignores_a_swipe_with_a_negative_duration() {
        assert_eq!(hid_timeout(&[swipe_event(-5.0)]), RPC_TIMEOUT);
    }

    #[test]
    fn connect_to_missing_socket_errors_cleanly() {
        // A UDS path that does not exist -> a structured Backend error, no panic. Runs on Linux.
        let err = IdbClient::connect(std::path::Path::new("/nonexistent/idb.sock")).unwrap_err();
        assert!(matches!(err, glass_core::GlassError::Backend(_)), "{err:?}");
    }

    #[test]
    fn dead_socket_is_handled_cleanly_at_connect_or_first_rpc() {
        // A stale socket file — the companion died, leaving its socket behind. Whether the
        // failure surfaces at `connect` or at the first RPC is a tonic/HTTP-2 timing detail:
        // `connect` can return `Ok` once the stream dials and the client preface is buffered,
        // before the peer's reset is observed. `connect` makes no promise to reject a dead
        // socket — liveness is really validated earlier (the companion's `await_socket` at
        // spawn) and backstopped by `RPC_TIMEOUT` on the first RPC. What we guarantee, and
        // assert here deterministically so it can't flake on connect's timing, is that a dead
        // socket is handled cleanly: a structured `Backend` error, never a panic, at one of
        // those two points (`map_timed` folds every RPC transport error/timeout into `Backend`).
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("idb.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        }
        match IdbClient::connect(&sock) {
            // Refused at the transport layer — the common case.
            Err(GlassError::Backend(_)) => {}
            // `connect` optimistically succeeded; the first RPC must then fail cleanly.
            Ok(client) => assert!(
                matches!(client.describe_all(), Err(GlassError::Backend(_))),
                "first RPC on a dead socket must be a clean Backend error"
            ),
            Err(other) => panic!("unexpected non-Backend error from connect: {other:?}"),
        }
    }
}
