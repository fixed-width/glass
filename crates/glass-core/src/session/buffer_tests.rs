use super::*;
use crate::session::test_support::{FakePlatform, spec};
use crate::{RecordedVideo, RecordingArtifact, RecordingFormat, RecordingSink};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

type Events = Arc<Mutex<Vec<&'static str>>>;
struct Buffer {
    events: Events,
    pending: Option<RollingWindow>,
    now: u64,
}
fn movie() -> Vec<u8> {
    let mut bytes = Vec::new();
    for (kind, data) in [
        (b"ftyp", b"isom".as_slice()),
        (b"mdat", b"frame".as_slice()),
        (b"moov", b"index".as_slice()),
    ] {
        bytes.extend(((data.len() + 8) as u32).to_be_bytes());
        bytes.extend(kind);
        bytes.extend(data);
    }
    bytes
}
impl RollingBuffer for Buffer {
    fn progress(&self) -> RollingProgress {
        RollingProgress {
            state: RollingState::Buffering,
            elapsed_ms: self.now,
            retained_ms: 1000,
            bytes: 50,
            pending_bytes: 50,
            truncated: false,
            reason: None,
            pending: self.pending.clone(),
        }
    }
    fn mark(&mut self, id: &str, tail_ms: u64, _: Deadline) -> Result<RollingWindow> {
        self.events.lock().unwrap().push("mark");
        self.now += 100;
        let window = RollingWindow {
            id: id.into(),
            start_ms: 0,
            end_ms: self.now + tail_ms,
            marked_at_ms: self.now,
            captured_until_ms: self.now,
            bytes: 50,
            ready: tail_ms == 0,
            truncated: false,
            reason: None,
        };
        self.pending = Some(window.clone());
        Ok(window)
    }
    fn save(&mut self, _: Deadline) -> Result<BufferedVideo> {
        self.events.lock().unwrap().push("export");
        let window = self.pending.take().ok_or_else(|| error("no mark"))?;
        Ok(BufferedVideo {
            video: RecordedVideo {
                format: RecordingFormat::Mp4,
                progress: progress(&window, RollingState::Buffering),
                data: movie(),
            },
            window,
        })
    }
    fn stop(&mut self, keep: bool, deadline: Deadline) -> Result<Option<BufferedVideo>> {
        self.events.lock().unwrap().push("buffer_stop");
        if keep && self.pending.is_some() {
            self.save(deadline).map(Some)
        } else {
            self.pending = None;
            Ok(None)
        }
    }
}
struct Sink {
    saved: Arc<Mutex<Vec<RecordingSummary>>>,
    next: AtomicUsize,
}
impl RecordingSink for Sink {
    fn next_id(&self) -> Result<String> {
        Ok(format!("{:032x}", self.next.fetch_add(1, Ordering::SeqCst)))
    }
    fn save(
        &mut self,
        summary: &RecordingSummary,
        video: &RecordedVideo,
    ) -> Result<RecordingArtifact> {
        self.saved.lock().unwrap().push(summary.clone());
        Ok(RecordingArtifact {
            uri: format!("test://{}", summary.id),
            path: "clip.mp4".into(),
            path_scope: "server".into(),
            mime_type: "video/mp4".into(),
            bytes: video.data.len() as u64,
            sha256: "digest".into(),
        })
    }
}
fn setup() -> (
    Glass,
    Events,
    Arc<Mutex<Vec<RecordingSummary>>>,
    tempfile::TempDir,
) {
    setup_with_stops(Arc::new(Mutex::new(0)))
}

fn setup_with_stops(
    stops: Arc<Mutex<u32>>,
) -> (
    Glass,
    Events,
    Arc<Mutex<Vec<RecordingSummary>>>,
    tempfile::TempDir,
) {
    let root = tempfile::tempdir().unwrap();
    let events = Events::default();
    let log = Arc::clone(&events);
    let stop_log = Arc::clone(&stops);
    let mut glass = Glass::new(
        Box::new(move |_| {
            Ok(Backend::display_only(Box::new(
                FakePlatform::new(64, 48)
                    .counting_stops(Arc::clone(&stop_log))
                    .with_lifecycle_log(Arc::clone(&log))
                    .with_rolling_buffer(Box::new(Buffer {
                        events: Arc::clone(&log),
                        pending: None,
                        now: 1000,
                    })),
            )))
        }),
        "test".into(),
        BaselineStore::new(root.path()),
        100,
    );
    let saved = Arc::new(Mutex::new(Vec::new()));
    glass
        .set_recording_sink(Box::new(Sink {
            saved: Arc::clone(&saved),
            next: AtomicUsize::new(1),
        }))
        .unwrap();
    glass.start(&spec()).unwrap();
    (glass, events, saved, root)
}

#[test]
fn ignored_successes_do_not_change_kept_marks_and_merged_metadata_has_one_artifact() {
    let (mut glass, events, saved, _root) = setup();
    glass.start_rolling_buffer(None).unwrap();
    assert!(
        glass
            .mark_rolling_buffer("ordinary success", RecordingOutcome::Success, None, None)
            .unwrap()
            .is_none()
    );
    assert!(!events.lock().unwrap().contains(&"mark"));
    let first = glass
        .mark_rolling_buffer(
            "selected success",
            RecordingOutcome::Success,
            Some(true),
            Some(0),
        )
        .unwrap()
        .unwrap();
    let second = glass
        .mark_rolling_buffer(
            "failed verification",
            RecordingOutcome::Failure,
            None,
            Some(0),
        )
        .unwrap()
        .unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(second.marks.len(), 2);
    assert_eq!(second.outcome, Some(RecordingOutcome::Failure));
    glass
        .mark_rolling_buffer(
            "ignored cancellation",
            RecordingOutcome::Cancelled,
            Some(false),
            None,
        )
        .unwrap();
    let exported = glass.save_rolling_buffer(Deadline::UNBOUNDED).unwrap();
    assert!(exported.kept);
    assert_eq!(saved.lock().unwrap().len(), 1);
    assert_eq!(saved.lock().unwrap()[0].marks.len(), 2);
    assert!(glass.recording_status().buffer.unwrap().pending.is_none());
    glass
        .stop_rolling_buffer(None, Deadline::UNBOUNDED)
        .unwrap();
}

#[test]
fn stopping_without_marks_writes_nothing_and_capture_modes_are_exclusive() {
    let (mut glass, _, saved, _root) = setup();
    glass.start_rolling_buffer(None).unwrap();
    assert!(
        glass
            .start_rolling_buffer(None)
            .unwrap_err()
            .to_string()
            .contains("already active")
    );
    assert!(glass.start_recording("competing", None).is_err());
    glass.stop().unwrap();
    assert!(saved.lock().unwrap().is_empty());
    assert!(glass.recording_status().buffer.is_none());
}

#[test]
fn application_replacement_exports_marked_evidence_before_stopping_the_old_app() {
    let (mut glass, events, saved, _root) = setup();
    glass.start_rolling_buffer(None).unwrap();
    glass
        .mark_rolling_buffer("failure", RecordingOutcome::Failure, None, None)
        .unwrap();
    glass.start(&spec()).unwrap();
    assert_eq!(saved.lock().unwrap().len(), 1);
    let events = events.lock().unwrap();
    let stop = events.iter().position(|e| *e == "buffer_stop").unwrap();
    let app = events.iter().rposition(|e| *e == "start").unwrap();
    assert!(stop < app);
}

#[test]
fn explicit_discard_and_invalid_marks_never_persist_a_clip() {
    let (mut glass, _, saved, _root) = setup();
    glass.start_rolling_buffer(None).unwrap();
    assert!(
        glass
            .mark_rolling_buffer("", RecordingOutcome::Failure, None, None)
            .is_err()
    );
    assert!(
        glass
            .mark_rolling_buffer("bad tail", RecordingOutcome::Failure, None, Some(5001))
            .is_err()
    );
    glass
        .mark_rolling_buffer("failure", RecordingOutcome::Failure, None, Some(0))
        .unwrap();
    glass
        .stop_rolling_buffer(Some(false), Deadline::UNBOUNDED)
        .unwrap();
    assert!(saved.lock().unwrap().is_empty());
    assert_eq!(
        glass.recording_status().last.unwrap().progress.state,
        RecordingState::Discarded
    );
}

#[test]
fn marked_history_is_finalized_before_cleanup_and_persisted_after_the_shutdown_hook() {
    let stops = Arc::new(Mutex::new(0));
    let (mut glass, events, _, _root) = setup_with_stops(stops.clone());
    let (entered, writing) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    glass.recordings.sink = Some(Box::new(
        crate::session::test_support::BlockedRecordingSink {
            entered,
            release: blocked,
        },
    ));
    glass.start_rolling_buffer(None).unwrap();
    glass
        .mark_rolling_buffer("incident", RecordingOutcome::Failure, None, None)
        .unwrap();
    let hooked = events.clone();
    glass.set_shutdown_hook(Box::new(move |_| hooked.lock().unwrap().push("hook")));
    let observer = std::thread::spawn(move || {
        writing
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let during_write = events.lock().unwrap().clone();
        release.send(()).unwrap();
        (during_write, *stops.lock().unwrap())
    });
    glass.shutdown_report(Deadline::from_millis(1000)).unwrap();
    let (during_write, stops) = observer.join().unwrap();
    let finish = during_write.iter().position(|e| *e == "export").unwrap();
    let hook = during_write.iter().position(|e| *e == "hook").unwrap();
    assert_eq!(stops, 1);
    assert!(finish < hook, "{during_write:?}");
    assert!(glass.recording_status().buffer.is_none());
    assert_eq!(
        glass.recording_status().last.unwrap().progress.state,
        RecordingState::Failed
    );
}

#[test]
fn mark_names_tails_and_count_are_bounded_without_losing_a_pending_incident() {
    let (mut glass, events, _, _root) = setup();
    glass.start_rolling_buffer(None).unwrap();
    for name in ["x".repeat(121), "x".repeat(122), "bad\tname".into()] {
        assert!(
            glass
                .mark_rolling_buffer(&name, RecordingOutcome::Failure, None, Some(0))
                .is_err()
        );
    }
    assert!(
        glass
            .mark_rolling_buffer("bad tail", RecordingOutcome::Failure, None, Some(5001))
            .is_err()
    );
    assert!(!events.lock().unwrap().contains(&"mark"));
    let first = glass
        .mark_rolling_buffer(
            &"x".repeat(120),
            RecordingOutcome::Failure,
            None,
            Some(5000),
        )
        .unwrap()
        .unwrap();
    assert_eq!(first.window.unwrap().end_ms, 6100);
    for _ in 1..BUFFER_MAX_MARKS {
        glass
            .mark_rolling_buffer("same incident", RecordingOutcome::Cancelled, None, Some(0))
            .unwrap();
    }
    assert!(
        glass
            .mark_rolling_buffer("one too many", RecordingOutcome::Failure, None, None)
            .is_err()
    );
    assert_eq!(
        glass
            .recording_status()
            .buffer
            .unwrap()
            .pending
            .unwrap()
            .marks
            .len(),
        BUFFER_MAX_MARKS
    );
}

#[test]
fn merged_outcomes_preserve_the_most_severe_classification_in_either_order() {
    for outcomes in [
        [RecordingOutcome::Failure, RecordingOutcome::Success],
        [RecordingOutcome::Cancelled, RecordingOutcome::Failure],
        [RecordingOutcome::Cancelled, RecordingOutcome::Success],
        [RecordingOutcome::Interrupted, RecordingOutcome::Success],
        [RecordingOutcome::Success, RecordingOutcome::Success],
    ] {
        let (mut glass, _, _, _root) = setup();
        glass.start_rolling_buffer(None).unwrap();
        for outcome in outcomes {
            glass
                .mark_rolling_buffer("incident", outcome, Some(true), Some(0))
                .unwrap();
        }
        let pending = glass.recording_status().buffer.unwrap().pending.unwrap();
        assert_eq!(
            pending.outcome,
            Some(if outcomes.contains(&RecordingOutcome::Failure) {
                RecordingOutcome::Failure
            } else if outcomes.contains(&RecordingOutcome::Cancelled) {
                RecordingOutcome::Cancelled
            } else if outcomes.contains(&RecordingOutcome::Interrupted) {
                RecordingOutcome::Interrupted
            } else {
                RecordingOutcome::Success
            })
        );
    }
}

#[test]
fn rolling_progress_distinguishes_failure_limits_readiness_and_capture() {
    let mut window = RollingWindow {
        id: "0123456789abcdef0123456789abcdef".into(),
        start_ms: 10,
        end_ms: 40,
        marked_at_ms: 20,
        captured_until_ms: 30,
        bytes: 50,
        ready: false,
        truncated: false,
        reason: None,
    };
    assert_eq!(
        progress(&window, RollingState::Buffering).state,
        RecordingState::Recording
    );
    assert_eq!(
        progress(&window, RollingState::Failed).state,
        RecordingState::Failed
    );
    window.ready = true;
    assert_eq!(
        progress(&window, RollingState::Buffering).state,
        RecordingState::Finalized
    );
    window.reason = Some("byte_limit".into());
    let limited = progress(&window, RollingState::Buffering);
    assert_eq!(limited.state, RecordingState::Limited);
    assert_eq!((limited.bytes, limited.elapsed_ms), (50, 30));
}

#[test]
fn a_live_buffer_prevents_replacing_its_store_even_without_an_active_target() {
    let (mut glass, _, saved, _root) = setup();
    glass.start_rolling_buffer(None).unwrap();
    let session = glass.active.take();
    assert!(
        glass
            .set_recording_sink(Box::new(Sink {
                saved,
                next: AtomicUsize::new(1)
            }))
            .is_err()
    );
    glass.active = session;
    glass.stop().unwrap();
}

struct BrokenBuffer {
    base: Buffer,
    missing: bool,
}

impl RollingBuffer for BrokenBuffer {
    fn progress(&self) -> RollingProgress {
        self.base.progress()
    }
    fn mark(&mut self, id: &str, tail: u64, deadline: Deadline) -> Result<RollingWindow> {
        self.base.mark(id, tail, deadline)
    }
    fn save(&mut self, _: Deadline) -> Result<BufferedVideo> {
        Err(error("lost native recorder"))
    }
    fn stop(&mut self, _: bool, _: Deadline) -> Result<Option<BufferedVideo>> {
        if self.missing {
            Ok(None)
        } else {
            Err(error("lost native recorder"))
        }
    }
}

#[test]
fn missing_or_failed_native_disposition_never_claims_a_mark_was_discarded() {
    for missing in [false, true] {
        let (mut glass, events, saved, _root) = setup();
        glass.start_rolling_buffer(None).unwrap();
        glass
            .mark_rolling_buffer("incident", RecordingOutcome::Failure, None, None)
            .unwrap();
        let buffer = glass.recordings.buffer.as_mut().unwrap();
        buffer.handle = Box::new(BrokenBuffer {
            base: Buffer {
                events,
                now: 1000,
                pending: buffer.summary.progress.pending.clone(),
            },
            missing,
        });
        let failure = glass
            .stop_rolling_buffer(None, Deadline::UNBOUNDED)
            .unwrap_err();
        assert!(failure.to_string().contains(if missing {
            "did not match"
        } else {
            "lost native recorder"
        }));
        if !missing {
            let last = glass.recording_status().last.unwrap();
            assert_eq!(last.progress.state, RecordingState::Failed);
            assert!(
                last.progress
                    .reason
                    .unwrap()
                    .contains("lost native recorder")
            );
        }
        assert!(saved.lock().unwrap().is_empty());
        assert!(glass.recording_status().buffer.is_none());
    }
}
