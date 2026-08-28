//! Synchronous wrapper over idb's async gRPC `CompanionService`. glass-core's
//! `Platform`/`Accessibility` seams are synchronous, so this owns a private
//! current-thread tokio runtime and blocks on each RPC. Transport is a
//! Unix-domain socket that `idb_companion --grpc-domain-sock` listens on.
//!
//! See [`IdbClient`] for the threading invariant that `block_on` imposes, and
//! [`CONNECT_TIMEOUT`]/[`RPC_TIMEOUT`] for the deadlines that keep a wedged
//! companion from hanging the caller.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use glass_core::{Deadline, GlassError, Result, Whose};
use tokio::runtime::Runtime;
use tonic::transport::{Channel, Endpoint, Uri};

use super::proto;
use proto::companion_service_client::CompanionServiceClient;

/// Budget for establishing the connection (dial + HTTP/2 handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for a single RPC. 30s mirrors glass-android's socket timeout.
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
    /// The companion binary serving this channel, so an error can name what refused a call.
    bin: PathBuf,
    /// Read **when the companion was spawned**. Do not re-read it at error time: right after a
    /// user replaces the binary as the error advises, that reports a fresh build as evidence
    /// the old one is in the way.
    build_info: Option<String>,
}

/// Budget for one `hid` stream. A `HidSwipe`/`HidDelay`/`HidPinch` plays out over
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

fn map_rpc_outcome<T, E: std::fmt::Display>(
    op: &str,
    timeout: Duration,
    whose: Whose,
    ends: Instant,
    outcome: std::result::Result<std::result::Result<T, E>, tokio::time::error::Elapsed>,
) -> Result<T> {
    if Instant::now() >= ends {
        return Err(rpc_timeout_error(op, timeout, whose));
    }
    match outcome {
        Err(_) if whose == Whose::Caller => Err(GlassError::caller_deadline_elapsed(op)),
        other => map_timed(op, timeout, other),
    }
}

fn rpc_timeout_error(op: &str, timeout: Duration, whose: Whose) -> GlassError {
    match whose {
        Whose::Caller => GlassError::caller_deadline_elapsed(op),
        Whose::Callee => {
            GlassError::Backend(format!("{op} timed out after {}s", timeout.as_secs()))
        }
    }
}

fn rpc_bound(
    deadline: Deadline,
    own_timeout: Duration,
    op: &str,
) -> Result<(Instant, Duration, Whose)> {
    let started = Instant::now();
    let (ends, whose) = deadline.resolve(started + own_timeout);
    let timeout = ends.saturating_duration_since(started);
    if timeout.is_zero() {
        return Err(match whose {
            Whose::Caller => GlassError::deadline_not_started(op),
            Whose::Callee => rpc_timeout_error(op, own_timeout, whose),
        });
    }
    Ok((ends, timeout, whose))
}

fn rpc_not_started_error(op: &str, timeout: Duration, whose: Whose) -> GlassError {
    match whose {
        Whose::Caller => GlassError::deadline_not_started(op),
        Whose::Callee => rpc_timeout_error(op, timeout, whose),
    }
}

fn finish_rpc_result_at<T>(
    op: &str,
    timeout: Duration,
    whose: Whose,
    ends: Instant,
    observed_at: Instant,
    result: Result<T>,
) -> Result<T> {
    if observed_at >= ends {
        Err(rpc_timeout_error(op, timeout, whose))
    } else {
        result
    }
}

fn finish_rpc_result<T>(
    op: &str,
    timeout: Duration,
    whose: Whose,
    ends: Instant,
    result: Result<T>,
) -> Result<T> {
    finish_rpc_result_at(op, timeout, whose, ends, Instant::now(), result)
}

/// Fold one `hid` outcome into a `Result`. An event this build does not implement is reported
/// as that; everything else keeps the generic folding every other RPC gets.
///
/// Do not inline this back into [`IdbClient::hid`]: the predicate and the message were each
/// tested there while the choice between them was not, so the suite passed with the feature
/// gone.
fn map_hid_outcome<T>(
    bin: &Path,
    build_info: Option<&str>,
    timeout: Duration,
    whose: Whose,
    ends: Instant,
    outcome: std::result::Result<
        std::result::Result<T, tonic::Status>,
        tokio::time::error::Elapsed,
    >,
) -> Result<()> {
    if Instant::now() >= ends {
        return Err(rpc_timeout_error("idb hid", timeout, whose));
    }
    match outcome {
        Err(_) if whose == Whose::Caller => Err(GlassError::caller_deadline_elapsed("idb hid")),
        Ok(Err(status)) if unimplemented_event(&status) => Err(unimplemented_event_error_with(
            bin,
            &status,
            build_info.map(str::to_owned),
        )),
        other => map_timed("idb hid", timeout, other).map(|_| ()),
    }
}

/// Whether `status` is idb refusing an event its build does not implement.
///
/// Observed against companion 1.1.8: an unimplemented event and a malformed one both come back
/// as `InvalidArgument`, so only the marker text separates them. If that wording ever drifts
/// this answers `false` and the caller gets the generic folding, which still carries idb's own
/// message — a worse-framed cause, never a swallowed one.
fn unimplemented_event(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::InvalidArgument
        && status.message().contains("Unrecognized request.event")
}

/// The error for an event this companion does not implement, with its build info injected.
///
/// Reports what was observed: the binary that refused, and idb's own words. It does not say
/// which builds carry the event — this process cannot check that, and an unverifiable claim
/// cannot be kept true — so it points at the setup guide for where to get one instead.
fn unimplemented_event_error_with(
    bin: &Path,
    status: &tonic::Status,
    info: Option<String>,
) -> GlassError {
    let build = match info {
        Some(text) => format!(" Its --version reports: {text}."),
        None => String::new(),
    };
    GlassError::Backend(format!(
        "the idb_companion at {} does not implement this event: it answered {:?}.{build} \
         See docs/how-to/setup-ios.md for companion requirements.",
        bin.display(),
        status.message()
    ))
}

impl IdbClient {
    /// Connect to `idb_companion`'s gRPC over the Unix socket at `sock`.
    pub fn connect(sock: &Path, bin: &Path, build_info: Option<&str>) -> Result<IdbClient> {
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
            bin: bin.to_path_buf(),
            build_info: build_info.map(str::to_owned),
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
            bin: PathBuf::from("/nonexistent/idb_companion"),
            build_info: None,
        }
    }

    /// Describe the whole accessibility tree: idb's `accessibility_info` over the entire
    /// screen (no point) in the nested/hierarchical format. Returns the response `json`.
    pub fn describe_all_by(&self, deadline: Deadline) -> Result<String> {
        let op = "idb accessibility_info";
        let (ends, timeout, whose) = rpc_bound(deadline, RPC_TIMEOUT, op)?;
        self.ensure_off_runtime("idb accessibility_info")?;
        let req = proto::AccessibilityInfoRequest {
            point: None,
            format: proto::accessibility_info_request::Format::Nested as i32,
        };
        let mut client = self.client.clone();
        let outcome = self.rt.block_on(async move {
            if Instant::now() >= ends {
                return None;
            }
            Some(
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(ends),
                    client.accessibility_info(req),
                )
                .await,
            )
        });
        let outcome = outcome.ok_or_else(|| rpc_not_started_error(op, timeout, whose))?;
        let resp = map_rpc_outcome(op, timeout, whose, ends, outcome)?;
        finish_rpc_result(op, timeout, whose, ends, Ok(resp.into_inner().json))
    }

    /// Describe the target itself: its screen's pixel size, logical-point size and density.
    /// These are properties of the device, so unlike [`describe_all_by`](Self::describe_all_by)
    /// they are available before the app under test has rendered anything.
    pub fn describe(&self) -> Result<proto::ScreenDimensions> {
        self.describe_by(Deadline::UNBOUNDED)
    }

    pub fn describe_by(&self, deadline: Deadline) -> Result<proto::ScreenDimensions> {
        let op = "idb describe";
        let (ends, timeout, whose) = rpc_bound(deadline, RPC_TIMEOUT, op)?;
        self.ensure_off_runtime(op)?;
        let mut client = self.client.clone();
        let outcome = self.rt.block_on(async move {
            if Instant::now() >= ends {
                return None;
            }
            Some(
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(ends),
                    client.describe(proto::TargetDescriptionRequest {
                        fetch_diagnostics: false,
                    }),
                )
                .await,
            )
        });
        let outcome = outcome.ok_or_else(|| rpc_not_started_error(op, timeout, whose))?;
        let resp = map_rpc_outcome(op, timeout, whose, ends, outcome)?;
        let dimensions = resp
            .into_inner()
            .target_description
            .and_then(|t| t.screen_dimensions)
            .ok_or_else(|| {
                GlassError::Backend("idb describe returned no screen dimensions".into())
            });
        finish_rpc_result(op, timeout, whose, ends, dimensions)
    }

    /// Send one HID event stream (client-streaming `hid`). A tap is two events
    /// (touch DOWN, touch UP); a chord is modifier + key down/up pairs.
    pub fn hid_by(&self, events: Vec<proto::HidEvent>, deadline: Deadline) -> Result<()> {
        // Derive the deadline from the gesture's own duration so a long swipe isn't
        // aborted mid-stream by a flat timeout (issue #116); see [`hid_timeout`].
        let own_timeout = hid_timeout(&events);
        let op = "idb hid";
        let (ends, timeout, whose) = rpc_bound(deadline, own_timeout, op)?;
        self.ensure_off_runtime(op)?;
        let mut client = self.client.clone();
        let outcome = self.rt.block_on(async move {
            if Instant::now() >= ends {
                return None;
            }
            let stream = tokio_stream::iter(events);
            Some(
                tokio::time::timeout_at(tokio::time::Instant::from_std(ends), client.hid(stream))
                    .await,
            )
        });
        let outcome = outcome.ok_or_else(|| rpc_not_started_error(op, timeout, whose))?;
        // Swapping this line for a bare `map_timed` compiles and keeps every off-device test
        // green; only `a_pinch_is_accepted_by_a_companion_that_implements_it`, run on-box
        // against a companion that predates the event, catches it.
        map_hid_outcome(
            &self.bin,
            self.build_info.as_deref(),
            timeout,
            whose,
            ends,
            outcome,
        )
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

    /// The acceptance leg for glass#117: a pinch built by the injector is accepted and executed
    /// by a real companion. CI cannot run this — it needs a booted Simulator and a companion
    /// that implements the event.
    ///
    /// Point `GLASS_IDB_COMPANION` at a companion that implements the event. To confirm the
    /// pinch also *lands* as two contacts, run it with a foreground app whose touch handler
    /// logs `event.allTouches.count` and watch for two.
    ///
    /// ```sh
    /// GLASS_IOS_UDID=<udid> GLASS_IDB_COMPANION=<path> \
    ///   cargo test -p glass-ios --lib a_pinch_is_accepted -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "on-box only: needs a booted iOS Simulator and a companion that implements HIDPinch"]
    fn a_pinch_is_accepted_by_a_companion_that_implements_it() {
        use crate::injector::IdbInjector;
        use glass_core::{PointerEvent, Segment};

        let udid = std::env::var("GLASS_IOS_UDID").expect("set GLASS_IOS_UDID to a booted sim");
        let companion =
            crate::idb::companion::IdbCompanion::spawn(&udid).expect("idb_companion must start");
        let client =
            IdbClient::connect(companion.socket(), companion.bin(), companion.build_info())
                .expect("companion must accept a client");

        // A spread about the middle of an iPhone 17 in device px (1206x2622, scale 3): fingers
        // 240px either side of centre move out to 480px, a scale of 2.0.
        let events = IdbInjector::new(3.0)
            .pointer_events(&PointerEvent::Gesture {
                pointers: vec![
                    Segment {
                        from_x: 363,
                        from_y: 1311,
                        to_x: 123,
                        to_y: 1311,
                    },
                    Segment {
                        from_x: 843,
                        from_y: 1311,
                        to_x: 1083,
                        to_y: 1311,
                    },
                ],
                duration_ms: 1000,
            })
            .expect("a symmetric spread classifies as a pinch");

        client.hid_by(events, Deadline::UNBOUNDED).expect(
            "the companion must accept HIDPinch; a build that does not implement it is named \
             in this error along with its --version output",
        );
    }

    /// The scale glass drives every tap at comes from this RPC (`platform::discover_scale`),
    /// so a target that stops reporting a usable density silently breaks input placement.
    /// That the value is *correct* is proved end-to-end by `tests/drive_integration.rs`,
    /// which taps elements by their accessibility bounds and asserts the app responded.
    ///
    /// ```sh
    /// GLASS_IOS_UDID=<udid> cargo test -p glass-ios --lib describe_reports -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "on-box only: needs a macOS host with a booted iOS Simulator and idb_companion"]
    fn describe_reports_a_usable_density_with_no_app_running() {
        let udid = std::env::var("GLASS_IOS_UDID").expect("set GLASS_IOS_UDID to a booted sim");
        let companion =
            crate::idb::companion::IdbCompanion::spawn(&udid).expect("idb_companion must start");
        let client =
            IdbClient::connect(companion.socket(), companion.bin(), companion.build_info())
                .expect("companion must accept a client");

        let d = client
            .describe()
            .expect("describe must report screen dimensions");
        println!(
            "pixels {}x{}, points {}x{}, density {}",
            d.width, d.height, d.width_points, d.height_points, d.density
        );
        assert!(
            d.density.is_finite() && d.density > 0.0,
            "a target reporting no usable density leaves glass unable to place input: {}",
            d.density
        );
    }

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
    fn a_read_rpc_success_observed_after_the_caller_deadline_is_not_success() {
        let error = map_rpc_outcome(
            "idb accessibility_info",
            Duration::from_millis(50),
            Whose::Caller,
            Instant::now() - Duration::from_millis(1),
            Ok::<std::result::Result<(), String>, _>(Ok(())),
        )
        .expect_err("runtime scheduling after the absolute deadline must not return read success");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn decoded_describe_success_observed_at_the_caller_deadline_is_not_success() {
        let ends = Instant::now();

        let error = finish_rpc_result_at(
            "idb describe",
            Duration::from_millis(5),
            Whose::Caller,
            ends,
            ends,
            Ok(()),
        )
        .expect_err("response decoding cannot return success at the absolute caller deadline");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn a_spent_describe_all_deadline_starts_no_rpc() {
        let client = IdbClient::for_test();

        let error = client
            .describe_all_by(Deadline::from_millis(0))
            .expect_err("a spent describe-all deadline must win before dialing the test channel");

        assert_eq!(error.bound(), Some(glass_core::BoundKind::NotStarted));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn a_spent_target_describe_deadline_starts_no_rpc() {
        let client = IdbClient::for_test();

        let error = client
            .describe_by(Deadline::from_millis(0))
            .expect_err("a spent target describe deadline must win before dialing");

        assert_eq!(error.bound(), Some(glass_core::BoundKind::NotStarted));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    fn elapsed() -> tokio::time::error::Elapsed {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a current-thread runtime");
        rt.block_on(async {
            tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>())
                .await
                .expect_err("a pending future must time out")
        })
    }

    #[test]
    fn an_unimplemented_event_is_routed_to_the_named_error() {
        // The routing itself: without this, deleting the arm that chooses leaves every other
        // test in this file passing.
        let outcome: std::result::Result<std::result::Result<(), _>, _> = Ok(Err(
            tonic::Status::invalid_argument("Unrecognized request.event"),
        ));
        let err = map_hid_outcome(
            std::path::Path::new("/somewhere/idb_companion"),
            Some("build 2022"),
            Duration::from_secs(30),
            Whose::Callee,
            Instant::now() + Duration::from_secs(30),
            outcome,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("does not implement"), "{msg}");
        assert!(
            msg.contains("build 2022"),
            "the spawn-time build info: {msg}"
        );
    }

    #[test]
    fn any_other_status_keeps_the_generic_folding() {
        // The guard must not annex genuine argument errors — they keep idb's own message.
        let outcome: std::result::Result<std::result::Result<(), _>, _> = Ok(Err(
            tonic::Status::invalid_argument("Unrecognized press.direction"),
        ));
        let err = map_hid_outcome(
            std::path::Path::new("/somewhere/idb_companion"),
            None,
            Duration::from_secs(30),
            Whose::Callee,
            Instant::now() + Duration::from_secs(30),
            outcome,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("idb hid:"),
            "the generic folding's own prefix: {msg}"
        );
        assert!(msg.contains("press.direction"), "{msg}");
        assert!(!msg.contains("does not implement"), "{msg}");
    }

    #[test]
    fn a_blown_deadline_still_reports_as_a_timeout() {
        let err = map_hid_outcome(
            std::path::Path::new("/somewhere/idb_companion"),
            None,
            Duration::from_secs(30),
            Whose::Callee,
            Instant::now() + Duration::from_secs(30),
            Err::<std::result::Result<(), tonic::Status>, _>(elapsed()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out after 30s"), "{err}");
    }

    #[test]
    fn a_caller_owned_hid_timeout_is_bounded_by_the_caller() {
        let error = map_hid_outcome(
            Path::new("/somewhere/idb_companion"),
            None,
            Duration::from_millis(50),
            Whose::Caller,
            Instant::now() + Duration::from_millis(50),
            Err::<std::result::Result<(), tonic::Status>, _>(elapsed()),
        )
        .unwrap_err();
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
    }

    #[test]
    fn a_hid_success_observed_after_the_caller_deadline_is_not_success() {
        let error = map_hid_outcome(
            Path::new("/somewhere/idb_companion"),
            None,
            Duration::from_millis(50),
            Whose::Caller,
            Instant::now() - Duration::from_millis(1),
            Ok::<std::result::Result<(), tonic::Status>, _>(Ok(())),
        )
        .expect_err("runtime scheduling after the absolute deadline must not return HID success");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn a_read_rpc_success_observed_after_its_backend_ceiling_is_not_success() {
        let error = map_rpc_outcome(
            "idb accessibility_info",
            Duration::from_secs(30),
            Whose::Callee,
            Instant::now() - Duration::from_millis(1),
            Ok::<std::result::Result<(), String>, _>(Ok(())),
        )
        .expect_err("runtime scheduling must not turn a spent backend ceiling into success");

        assert_eq!(error.bound_owner(), None);
        assert!(error.to_string().contains("timed out after 30s"), "{error}");
    }

    #[test]
    fn an_unrecognized_event_is_recognised_as_an_unimplemented_one() {
        let status = tonic::Status::invalid_argument("Unrecognized request.event");
        assert!(unimplemented_event(&status));
    }

    #[test]
    fn another_invalid_argument_is_not_mistaken_for_an_unimplemented_event() {
        // A genuine argument error must keep its own message rather than being reported as
        // an out-of-date companion.
        let status = tonic::Status::invalid_argument("Unrecognized press.direction");
        assert!(!unimplemented_event(&status));
    }

    #[test]
    fn a_different_code_carrying_the_same_text_is_not_an_unimplemented_event() {
        let status = tonic::Status::internal("Unrecognized request.event");
        assert!(!unimplemented_event(&status));
    }

    #[test]
    fn the_error_carries_the_build_info_when_it_is_available() {
        let err = unimplemented_event_error_with(
            std::path::Path::new("/somewhere/idb_companion"),
            &tonic::Status::invalid_argument("Unrecognized request.event"),
            Some("{\"build_date\":\"Aug 12 2022\"}".to_string()),
        );
        assert!(err.to_string().contains("Aug 12 2022"), "{err}");
    }

    #[test]
    fn the_error_stands_alone_when_the_build_info_is_unavailable() {
        let err = unimplemented_event_error_with(
            std::path::Path::new("/somewhere/idb_companion"),
            &tonic::Status::invalid_argument("Unrecognized request.event"),
            None,
        );
        let msg = err.to_string();
        assert!(msg.contains("does not implement"), "{msg}");
        assert!(
            !msg.contains("--version"),
            "no dangling half-sentence: {msg}"
        );
    }

    #[test]
    fn the_unimplemented_event_error_names_the_binary_that_refused_it() {
        let err = unimplemented_event_error_with(
            std::path::Path::new("/somewhere/idb_companion"),
            &tonic::Status::invalid_argument("Unrecognized request.event"),
            None,
        );
        let msg = err.to_string();
        assert!(msg.contains("/somewhere/idb_companion"), "{msg}");
        assert!(msg.contains("does not implement"), "{msg}");
        assert!(
            msg.contains("Unrecognized request.event"),
            "the cause is quoted: {msg}"
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
    fn idb_hid_uses_the_nearer_of_gesture_budget_and_caller_deadline() {
        let now = std::time::Instant::now();
        let caller = Deadline::at(now + Duration::from_millis(250));
        let (budget, whose) = caller.budget(RPC_TIMEOUT, now);
        assert_eq!(budget, Duration::from_millis(250));
        assert_eq!(whose, Whose::Caller);

        let (budget, whose) = Deadline::UNBOUNDED.budget(RPC_TIMEOUT, now);
        assert_eq!(budget, RPC_TIMEOUT);
        assert_eq!(whose, Whose::Callee);
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
        let err = IdbClient::connect(
            std::path::Path::new("/nonexistent/idb.sock"),
            std::path::Path::new("/nonexistent/idb_companion"),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, glass_core::GlassError::Backend(_)), "{err:?}");
    }

    #[test]
    fn dead_socket_is_handled_cleanly_at_connect_or_first_rpc() {
        // A stale socket file — the companion died, leaving its socket behind. Whether the
        // failure surfaces at `connect` or at the first RPC is a tonic/HTTP-2 timing detail:
        // `connect` can return `Ok` once the stream dials and the client preface is buffered,
        // before the peer's reset is observed. Liveness is really validated earlier (the
        // companion's `await_socket` at spawn) and backstopped by `RPC_TIMEOUT`.
        //
        // Asserted deterministically so it can't flake on connect's timing: a dead socket
        // yields a structured `Backend` error, never a panic, at one of those two points.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("idb.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        }
        match IdbClient::connect(
            &sock,
            std::path::Path::new("/nonexistent/idb_companion"),
            None,
        ) {
            // Refused at the transport layer — the common case.
            Err(GlassError::Backend(_)) => {}
            // `connect` optimistically succeeded; the first RPC must then fail cleanly.
            Ok(client) => assert!(
                matches!(
                    client.describe_all_by(Deadline::UNBOUNDED),
                    Err(GlassError::Backend(_))
                ),
                "first RPC on a dead socket must be a clean Backend error"
            ),
            Err(other) => panic!("unexpected non-Backend error from connect: {other:?}"),
        }
    }
}
