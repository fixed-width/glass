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
    stops: Arc<Mutex<u32>>,
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
        assert_eq!(
            *self.stops.lock().unwrap(),
            0,
            "marked evidence must be saved before app teardown"
        );
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
    let root = tempfile::tempdir().unwrap();
    let events = Events::default();
    let log = Arc::clone(&events);
    let stops = Arc::new(Mutex::new(0));
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
            stops,
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
    assert!(glass.start_rolling_buffer(None).is_err());
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
