use super::*;
use crate::recording::{
    ActiveRecording, RECORDING_DEFAULT_DURATION_MS, RECORDING_MAX_BYTES, RecordedVideo,
    RecordingOptions, RecordingOutcome, RecordingSink, RecordingState, RecordingStatus,
    RecordingSummary, validate_video,
};

impl Glass {
    pub fn set_recording_sink(&mut self, sink: Box<dyn RecordingSink>) -> Result<()> {
        if self.active.is_some()
            || self.recordings.active.is_some()
            || self.recordings.buffer.is_some()
        {
            return Err(GlassError::ProtectedPathsWhileActive);
        }
        self.recordings.sink = Some(sink);
        Ok(())
    }

    pub fn recording_status(&self) -> RecordingStatus {
        let active = self.recordings.active.as_ref().map(|active| {
            let mut summary = active.summary.clone();
            summary.progress = active.handle.progress();
            summary
        });
        RecordingStatus {
            enabled: self.recordings.sink.is_some(),
            supported: self
                .active
                .as_ref()
                .is_some_and(|s| s.platform.recording_supported()),
            active,
            last: self.recordings.last.clone(),
            buffer_supported: self
                .active
                .as_ref()
                .is_some_and(|s| s.platform.rolling_buffer_supported()),
            buffer: self.rolling_summary(),
        }
    }

    pub fn start_recording(
        &mut self,
        name: &str,
        duration_ms: Option<u64>,
    ) -> Result<RecordingSummary> {
        if name.trim().is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
            return Err(GlassError::Backend(
                "recording name must be nonblank, at most 120 bytes, without control characters"
                    .into(),
            ));
        }
        let sink =
            self.recordings
                .sink
                .as_ref()
                .ok_or_else(|| GlassError::UnsupportedOperation {
                    operation: "workflow recording".into(),
                    reason: "start the server with --record-dir to enable video evidence".into(),
                })?;
        if self.recordings.active.is_some() {
            return Err(GlassError::Backend(
                "a recording already awaits stop and disposition".into(),
            ));
        }
        if self.recordings.buffer.is_some() {
            return Err(GlassError::Backend(
                "stop the rolling buffer before starting a scoped recording".into(),
            ));
        }
        let session = self.active.as_ref().ok_or(GlassError::NoActiveSession)?;
        if !session.platform.recording_supported() {
            return Err(GlassError::UnsupportedOperation {
                operation: "workflow recording".into(),
                reason: "this backend has no recording adapter".into(),
            });
        }
        let options = RecordingOptions {
            id: sink.next_id()?,
            max_duration_ms: duration_ms.unwrap_or(RECORDING_DEFAULT_DURATION_MS),
            max_bytes: RECORDING_MAX_BYTES,
        };
        options.validate()?;
        let id = options.id.clone();
        let session = self.active_mut()?;
        let handle = session
            .platform
            .start_recording(options, Deadline::from_millis(15_000))?;
        let summary = RecordingSummary {
            id,
            name: name.to_owned(),
            backend: session.backend.clone(),
            scope: session.platform.recording_scope().as_str().into(),
            progress: handle.progress(),
            outcome: None,
            kept: false,
            artifact: None,
            marks: Vec::new(),
            window: None,
        };
        self.recordings.active = Some(ActiveRecording {
            summary: summary.clone(),
            handle,
        });
        Ok(summary)
    }

    pub fn stop_recording(
        &mut self,
        outcome: RecordingOutcome,
        keep: Option<bool>,
        deadline: Deadline,
    ) -> Result<RecordingSummary> {
        let mut active = self
            .recordings
            .active
            .take()
            .ok_or_else(|| GlassError::Backend("no recording awaits disposition".into()))?;
        let keep = keep.unwrap_or(outcome != RecordingOutcome::Success);
        active.summary.outcome = Some(outcome);
        let finished = active
            .handle
            .finish(keep, deadline)
            .and_then(|video: RecordedVideo| {
                active.summary.progress = video.progress.clone();
                if keep {
                    if video.data.len() as u64 > RECORDING_MAX_BYTES {
                        return Err(GlassError::Backend(
                            "recording exceeds the clip byte limit".into(),
                        ));
                    }
                    validate_video(&video.data)?;
                    active.summary.kept = true;
                    active.summary.artifact = Some(
                        self.recordings
                            .sink
                            .as_mut()
                            .ok_or_else(|| {
                                GlassError::Backend("recording storage unavailable".into())
                            })?
                            .save(&active.summary, &video)?,
                    );
                } else {
                    active.summary.progress.state = RecordingState::Discarded;
                }
                Ok(())
            });
        if let Err(error) = &finished {
            active.summary.progress = active.handle.progress();
            active.summary.kept = false;
            active.summary.progress.state = RecordingState::Failed;
            active.summary.progress.reason = Some(error.to_string());
        }
        self.recordings.last = Some(active.summary.clone());
        finished.map(|()| active.summary)
    }

    pub(super) fn finish_recording_for_stop(&mut self, deadline: Deadline) {
        if self.recordings.buffer.is_some() {
            let _ = self.stop_rolling_buffer(None, deadline);
        }
        if self.recordings.active.is_some() {
            let _ = self.stop_recording(RecordingOutcome::Interrupted, Some(true), deadline);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{RecordingArtifact, RecordingFormat, RecordingProgress};
    use crate::session::test_support::{FakePlatform, spec};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    type Events = Arc<Mutex<Vec<&'static str>>>;
    struct Source {
        events: Events,
        fail: bool,
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
    impl crate::Recording for Source {
        fn progress(&self) -> RecordingProgress {
            RecordingProgress {
                state: RecordingState::Recording,
                elapsed_ms: 1,
                bytes: 42,
                reason: None,
            }
        }
        fn finish(&mut self, keep: bool, _: Deadline) -> Result<RecordedVideo> {
            self.events.lock().unwrap().push("record_finish");
            if self.fail {
                return Err(GlassError::Backend("encoder failure".into()));
            }
            Ok(RecordedVideo {
                format: RecordingFormat::Mp4,
                progress: self.progress(),
                data: if keep { movie() } else { vec![] },
            })
        }
    }
    struct TestPlatform {
        base: FakePlatform,
        events: Events,
        fail: bool,
    }
    impl Platform for TestPlatform {
        fn recording_supported(&self) -> bool {
            true
        }
        fn start_recording(
            &mut self,
            _: RecordingOptions,
            _: Deadline,
        ) -> Result<Box<dyn crate::Recording>> {
            self.events.lock().unwrap().push("record_start");
            Ok(Box::new(Source {
                events: self.events.clone(),
                fail: self.fail,
            }))
        }
        fn start_app(&mut self, s: &AppSpec) -> Result<WindowGeometry> {
            self.events.lock().unwrap().push("app_start");
            self.base.start_app(s)
        }
        fn stop_app_by(&mut self, d: Deadline) -> Result<()> {
            self.events.lock().unwrap().push("app_stop");
            self.base.stop_app_by(d)
        }
        fn capture_frame_by(&mut self, r: Option<&Region>, d: Deadline) -> Result<Frame> {
            self.base.capture_frame_by(r, d)
        }
        fn capture_window_by(
            &mut self,
            w: WindowId,
            r: Option<&Region>,
            d: Deadline,
        ) -> Result<Frame> {
            self.base.capture_window_by(w, r, d)
        }
        fn send_pointer_by(&mut self, e: &PointerEvent, d: Deadline) -> Result<()> {
            self.base.send_pointer_by(e, d)
        }
        fn send_key_by(&mut self, e: &KeyEvent, d: Deadline) -> Result<()> {
            self.base.send_key_by(e, d)
        }
        fn window_by(&mut self, o: &WindowOp, d: Deadline) -> Result<WindowGeometry> {
            self.base.window_by(o, d)
        }
        fn list_windows_by(&mut self, d: Deadline) -> Result<Vec<WindowInfo>> {
            self.base.list_windows_by(d)
        }
        fn select_window_by(&mut self, w: WindowId, d: Deadline) -> Result<WindowGeometry> {
            self.base.select_window_by(w, d)
        }
        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            self.base.drain_logs()
        }
    }
    struct Sink {
        events: Events,
        next: AtomicUsize,
    }
    impl RecordingSink for Sink {
        fn next_id(&self) -> Result<String> {
            Ok(format!("{:032x}", self.next.fetch_add(1, Ordering::SeqCst)))
        }
        fn save(&mut self, s: &RecordingSummary, v: &RecordedVideo) -> Result<RecordingArtifact> {
            self.events.lock().unwrap().push("save");
            Ok(RecordingArtifact {
                uri: format!("test://{}", s.id),
                path: "test.mp4".into(),
                path_scope: "server".into(),
                mime_type: v.format.mime_type().into(),
                bytes: v.data.len() as u64,
                sha256: "digest".into(),
            })
        }
    }
    fn setup(fail: bool) -> (Glass, Events, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let events = Events::default();
        let factory_events = events.clone();
        let mut glass = Glass::new(
            Box::new(move |_| {
                Ok(Backend::display_only(Box::new(TestPlatform {
                    base: FakePlatform::new(100, 100),
                    events: factory_events.clone(),
                    fail,
                })))
            }),
            "test".into(),
            BaselineStore::new(root.path()),
            100,
        );
        glass
            .set_recording_sink(Box::new(Sink {
                events: events.clone(),
                next: AtomicUsize::new(1),
            }))
            .unwrap();
        glass.start(&spec()).unwrap();
        (glass, events, root)
    }
    #[test]
    fn successful_workflows_are_discarded_and_selected_successes_and_failures_are_saved() {
        let (mut glass, events, _root) = setup(false);
        glass.start_recording("ordinary success", None).unwrap();
        let discarded = glass
            .stop_recording(RecordingOutcome::Success, None, Deadline::UNBOUNDED)
            .unwrap();
        assert!(!discarded.kept && discarded.progress.state == RecordingState::Discarded);
        glass.start_recording("failed save", None).unwrap();
        assert!(
            glass
                .stop_recording(RecordingOutcome::Failure, None, Deadline::UNBOUNDED)
                .unwrap()
                .kept
        );
        glass.start_recording("selected success", None).unwrap();
        assert!(
            glass
                .stop_recording(RecordingOutcome::Success, Some(true), Deadline::UNBOUNDED)
                .unwrap()
                .kept
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| **e == "save")
                .count(),
            2
        );
    }
    #[test]
    fn stop_and_replacement_finalize_before_releasing_the_application() {
        let (mut glass, events, _root) = setup(false);
        glass.start_recording("interrupted", None).unwrap();
        glass.start(&spec()).unwrap();
        assert_eq!(
            &*events.lock().unwrap(),
            &[
                "app_start",
                "record_start",
                "record_finish",
                "save",
                "app_stop",
                "app_start"
            ]
        );
        assert_eq!(
            glass.recording_status().last.unwrap().outcome,
            Some(RecordingOutcome::Interrupted)
        );
        glass.start_recording("stop", None).unwrap();
        glass.stop().unwrap();
        assert_eq!(events.lock().unwrap().last(), Some(&"app_stop"));
        assert!(glass.recording_status().last.unwrap().kept);
    }
    #[test]
    fn capture_failure_remains_inspectable_without_blocking_application_cleanup() {
        let (mut glass, events, _root) = setup(true);
        glass.start_recording("failure", None).unwrap();
        glass.stop().unwrap();
        let status = glass.recording_status();
        assert!(status.active.is_none());
        let last = status.last.unwrap();
        assert_eq!(last.progress.state, RecordingState::Failed);
        assert!(last.progress.reason.unwrap().contains("encoder failure"));
        assert_eq!(events.lock().unwrap().last(), Some(&"app_stop"));
    }
    #[test]
    fn invalid_or_duplicate_starts_do_not_create_native_recorders() {
        let (mut glass, events, _root) = setup(false);
        for name in ["", "  ", "bad\nname"] {
            assert!(glass.start_recording(name, None).is_err());
        }
        assert!(glass.start_recording("bad duration", Some(0)).is_err());
        glass.start_recording("one", None).unwrap();
        assert!(glass.start_recording("two", None).is_err());
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| **e == "record_start")
                .count(),
            1
        );
        glass.stop().unwrap();
    }

    #[test]
    fn disabled_recording_starts_nothing_and_shutdown_finalizes_before_the_hook() {
        let (mut glass, events, _root) = setup(false);
        let sink = glass.recordings.sink.take().unwrap();
        assert!(!glass.recording_status().enabled);
        assert!(glass.start_recording("disabled", None).is_err());
        assert_eq!(&*events.lock().unwrap(), &["app_start"]);
        glass.recordings.sink = Some(sink);
        glass.start_recording("shutdown", None).unwrap();
        let hook = events.clone();
        glass.set_shutdown_hook(Box::new(move |_| hook.lock().unwrap().push("hook")));
        glass.shutdown_report(Deadline::from_millis(1000)).unwrap();
        assert_eq!(
            &*events.lock().unwrap(),
            &[
                "app_start",
                "record_start",
                "record_finish",
                "save",
                "app_stop",
                "hook"
            ]
        );
    }
}
