use crate::{
    CAPTURE_BUDGET, CaptureFactory, FRAME_INTERVAL, history::History, movie::FrameEncoder,
};
use glass_core::{
    BufferedVideo, Deadline, GlassError, RecordedVideo, RecordingFormat, RecordingProgress,
    RecordingState, Result, RollingBuffer, RollingOptions, RollingProgress, RollingState,
    RollingWindow,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

enum Command {
    Mark {
        id: String,
        tail_ms: u64,
        deadline: Deadline,
        reply: mpsc::SyncSender<Result<RollingWindow>>,
    },
    Save {
        deadline: Deadline,
        reply: mpsc::SyncSender<Result<BufferedVideo>>,
    },
    Stop {
        keep: bool,
        reply: mpsc::SyncSender<Result<Option<BufferedVideo>>>,
    },
}

struct Shared {
    progress: Mutex<RollingProgress>,
    origin: Mutex<Option<Instant>>,
    cancelled: AtomicBool,
}

struct Handle {
    commands: mpsc::SyncSender<Command>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    finished: bool,
}

fn error(reason: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!("rolling video history: {reason}"))
}

/// Start bounded encoded history and acknowledge an encoded keyframe before returning.
pub fn start(
    options: RollingOptions,
    deadline: Deadline,
    setup: CaptureFactory,
) -> Result<Box<dyn RollingBuffer>> {
    options.validate()?;
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started("rolling history startup"));
    }
    let shared = Arc::new(Shared {
        progress: Mutex::new(RollingProgress {
            state: RollingState::Buffering,
            elapsed_ms: 0,
            retained_ms: 0,
            bytes: 0,
            pending_bytes: 0,
            truncated: true,
            reason: None,
            pending: None,
        }),
        origin: Mutex::new(None),
        cancelled: AtomicBool::new(false),
    });
    let (ready, started) = mpsc::sync_channel(1);
    let (commands, incoming) = mpsc::sync_channel(1);
    let background = Arc::clone(&shared);
    let thread = std::thread::Builder::new()
        .name("glass-video-history".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(options, setup, &background, incoming, ready.clone())
            }))
            .unwrap_or_else(|_| Err(error("capture worker panicked")));
            if let Err(failure) = result {
                if let Ok(mut progress) = background.progress.lock() {
                    progress.state = RollingState::Failed;
                    progress.reason = Some(failure.to_string());
                }
                let _ = ready.try_send(Err(failure.to_string()));
            }
        })
        .map_err(error)?;
    let handle = Handle {
        commands,
        shared,
        thread: Some(thread),
        finished: false,
    };
    match started.recv_timeout(
        deadline
            .remaining()
            .unwrap_or(Duration::from_secs(15))
            .min(Duration::from_secs(15)),
    ) {
        Ok(Ok(())) => Ok(Box::new(handle)),
        Ok(Err(failure)) => Err(error(failure)),
        Err(_) => Err(error("startup did not acknowledge an encoded keyframe")),
    }
}

impl Handle {
    fn send(&self, command: Command, deadline: Deadline) -> Result<()> {
        if self.finished || deadline.has_passed() {
            return Err(GlassError::deadline_not_started("buffer command"));
        }
        self.commands
            .try_send(command)
            .map_err(|_| error("capture worker is busy or unavailable"))
    }
}

fn receive<T>(reply: mpsc::Receiver<Result<T>>, deadline: Deadline) -> Result<T> {
    reply
        .recv_timeout(
            deadline
                .remaining()
                .unwrap_or(Duration::from_secs(20))
                .min(Duration::from_secs(20)),
        )
        .map_err(|_| error("command response deadline elapsed"))?
}

impl RollingBuffer for Handle {
    fn progress(&self) -> RollingProgress {
        let mut progress =
            self.shared
                .progress
                .lock()
                .map(|p| p.clone())
                .unwrap_or(RollingProgress {
                    state: RollingState::Failed,
                    elapsed_ms: 0,
                    retained_ms: 0,
                    bytes: 0,
                    pending_bytes: 0,
                    truncated: true,
                    reason: Some("buffer state unavailable".into()),
                    pending: None,
                });
        if progress.state == RollingState::Buffering
            && let Ok(origin) = self.shared.origin.lock()
            && let Some(origin) = *origin
        {
            progress.elapsed_ms = origin.elapsed().as_millis() as u64;
        }
        progress
    }
    fn mark(&mut self, clip_id: &str, tail_ms: u64, deadline: Deadline) -> Result<RollingWindow> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(
            Command::Mark {
                id: clip_id.into(),
                tail_ms,
                deadline,
                reply,
            },
            deadline,
        )?;
        receive(result, deadline)
    }
    fn save(&mut self, deadline: Deadline) -> Result<BufferedVideo> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::Save { deadline, reply }, deadline)?;
        receive(result, deadline)
    }
    fn stop(&mut self, keep: bool, deadline: Deadline) -> Result<Option<BufferedVideo>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::Stop { keep, reply }, deadline)?;
        let outcome = receive(result, deadline);
        self.finished = true;
        self.shared.cancelled.store(true, Ordering::Release);
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
        outcome
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

fn export(
    history: &mut History,
    encoder: &FrameEncoder,
    now: u64,
    stopping: bool,
    max_bytes: u64,
    failed: bool,
) -> Result<BufferedVideo> {
    let (window, samples) = history.take(now, stopping)?;
    let data = encoder.mux(&samples, window.end_ms, max_bytes)?;
    let state = if failed {
        RecordingState::Failed
    } else if window.reason.is_some() {
        RecordingState::Limited
    } else {
        RecordingState::Finalized
    };
    Ok(BufferedVideo {
        video: RecordedVideo {
            format: RecordingFormat::Mp4,
            progress: RecordingProgress {
                state,
                elapsed_ms: window.end_ms.saturating_sub(window.start_ms),
                bytes: data.len() as u64,
                reason: window.reason.clone(),
            },
            data,
        },
        window,
    })
}

fn run(
    options: RollingOptions,
    setup: CaptureFactory,
    shared: &Shared,
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<std::result::Result<(), String>>,
) -> Result<()> {
    if shared.cancelled.load(Ordering::Acquire) {
        return Err(error("startup cancelled"));
    }
    let mut source = Some(setup()?);
    let first = source
        .as_mut()
        .ok_or_else(|| error("capture unavailable"))?(Deadline::at(
        Instant::now() + CAPTURE_BUDGET,
    ))?;
    let origin = Instant::now();
    let mut encoder = FrameEncoder::new(&first)?;
    let mut history = History::new(options.clone());
    let initial = encoder.encode(first, 0)?;
    if !initial.sync {
        return Err(error("encoder produced no initial keyframe"));
    }
    history.push(initial)?;
    if let Ok(mut value) = shared.origin.lock() {
        *value = Some(origin);
    }
    let mut failure: Option<String> = None;
    let mut saving: Option<(Deadline, mpsc::SyncSender<Result<BufferedVideo>>)> = None;
    let mut next = Instant::now() + FRAME_INTERVAL;
    publish(
        shared,
        &history,
        0,
        options.history_ms,
        failure.as_deref(),
        false,
    );
    let _ = ready.send(Ok(()));
    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let now = origin.elapsed().as_millis() as u64;
        if let Some((deadline, _)) = &saving {
            if deadline.has_passed() {
                if let Some((_, reply)) = saving.take() {
                    let _ = reply.send(Err(error("save deadline elapsed")));
                }
            } else if history.pending(now).is_some_and(|p| p.ready)
                && let Some((_, reply)) = saving.take()
            {
                let result = export(
                    &mut history,
                    &encoder,
                    now,
                    false,
                    options.max_bytes,
                    failure.is_some(),
                );
                publish(
                    shared,
                    &history,
                    now,
                    options.history_ms,
                    failure.as_deref(),
                    false,
                );
                let _ = reply.send(result);
            }
        }
        publish(
            shared,
            &history,
            now,
            options.history_ms,
            failure.as_deref(),
            false,
        );
        match commands.recv_timeout(Duration::from_millis(5)) {
            Ok(Command::Mark {
                id,
                tail_ms,
                deadline,
                reply,
            }) => {
                let result = if deadline.has_passed() || saving.is_some() {
                    Err(error("mark deadline elapsed or a save is in progress"))
                } else {
                    history
                        .mark(
                            &id,
                            if failure.is_some() {
                                history.latest_ms() + 1
                            } else {
                                now
                            },
                            if failure.is_some() { 0 } else { tail_ms },
                        )
                        .map(|window| {
                            if let Some(reason) = &failure {
                                history.fail(reason);
                            }
                            history.pending(now).unwrap_or(window)
                        })
                };
                publish(
                    shared,
                    &history,
                    now,
                    options.history_ms,
                    failure.as_deref(),
                    false,
                );
                let _ = reply.send(result);
            }
            Ok(Command::Save { deadline, reply }) => {
                if saving.is_some() || history.pending(now).is_none() {
                    let _ = reply.send(Err(error(
                        "no marked clip awaits saving, or another save is in progress",
                    )));
                } else {
                    saving = Some((deadline, reply));
                }
            }
            Ok(Command::Stop { keep, reply }) => {
                let result = if keep && history.pending(now).is_some() {
                    export(
                        &mut history,
                        &encoder,
                        now,
                        true,
                        options.max_bytes,
                        failure.is_some(),
                    )
                    .map(Some)
                } else {
                    history.discard();
                    Ok(None)
                };
                source.take();
                publish(
                    shared,
                    &history,
                    now,
                    options.history_ms,
                    failure.as_deref(),
                    true,
                );
                let _ = reply.send(result);
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next
            && let Some(capture) = source.as_mut()
        {
            let captured =
                capture(Deadline::at(Instant::now() + CAPTURE_BUDGET)).and_then(|frame| {
                    let now = origin.elapsed().as_millis() as u64;
                    if history.keyframe_due(now) {
                        encoder.force_keyframe();
                    }
                    let sample = encoder.encode(frame, now)?;
                    history.push(sample)
                });
            if let Err(error) = captured {
                let reason = format!("buffer capture failed: {error}");
                history.fail(&reason);
                failure = Some(reason);
                source.take();
            }
            next = Instant::now() + FRAME_INTERVAL;
        }
    }
}

fn publish(
    shared: &Shared,
    history: &History,
    now: u64,
    history_ms: u64,
    failure: Option<&str>,
    stopped: bool,
) {
    if let Ok(mut progress) = shared.progress.lock() {
        *progress = RollingProgress {
            state: if stopped {
                RollingState::Stopped
            } else if failure.is_some() {
                RollingState::Failed
            } else {
                RollingState::Buffering
            },
            elapsed_ms: now,
            retained_ms: history.retained_ms(now),
            bytes: history.bytes(),
            pending_bytes: history.pending_bytes(),
            truncated: history.retained_ms(now) < history_ms,
            reason: failure.map(str::to_owned),
            pending: history.pending(now),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::AtomicUsize;

    fn options() -> RollingOptions {
        RollingOptions {
            id: "a".repeat(32),
            history_ms: 1000,
            max_bytes: 8 * 1024 * 1024,
        }
    }
    fn source(counter: Arc<AtomicUsize>) -> CaptureFactory {
        Box::new(move || {
            Ok(Box::new(move |_| {
                let frame = counter.fetch_add(1, Ordering::AcqRel);
                glass_core::Frame::new(
                    64,
                    48,
                    [(frame % 16) as u8 * 13, 90, 120, 255].repeat(64 * 48),
                )
            }))
        })
    }

    fn until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn merged_incidents_export_pre_mark_history_and_tail_without_stopping_capture() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut buffer = start(
            options(),
            Deadline::from_millis(3000),
            source(Arc::clone(&counter)),
        )
        .unwrap();
        until(|| counter.load(Ordering::Acquire) >= 4);
        let first = buffer
            .mark(&"b".repeat(32), 200, Deadline::from_millis(1000))
            .unwrap();
        let merged = buffer
            .mark(&"b".repeat(32), 300, Deadline::from_millis(1000))
            .unwrap();
        assert_eq!(first.id, merged.id);
        assert!(merged.end_ms >= first.end_ms);
        let saved = buffer.save(Deadline::from_millis(3000)).unwrap();
        assert!(saved.window.start_ms < first.marked_at_ms);
        let mut movie = mp4::Mp4Reader::read_header(
            Cursor::new(&saved.video.data),
            saved.video.data.len() as u64,
        )
        .unwrap();
        assert!(movie.sample_count(1).unwrap() >= 4);
        let first_sample = movie.read_sample(1, 1).unwrap().unwrap();
        assert!(first_sample.is_sync);
        assert_eq!(first_sample.start_time, 0);
        assert!(saved.window.end_ms - saved.window.start_ms >= 300);
        assert!(buffer.progress().pending.is_none());
        let before = counter.load(Ordering::Acquire);
        until(|| counter.load(Ordering::Acquire) > before);
        assert!(
            buffer
                .stop(true, Deadline::from_millis(1000))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stopping_discards_unmarked_history_and_marks_can_be_explicitly_discarded() {
        let mut buffer = start(
            options(),
            Deadline::from_millis(3000),
            source(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap();
        assert!(
            buffer
                .stop(true, Deadline::from_millis(1000))
                .unwrap()
                .is_none()
        );
        let mut buffer = start(
            options(),
            Deadline::from_millis(3000),
            source(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap();
        buffer
            .mark(&"b".repeat(32), 5000, Deadline::from_millis(1000))
            .unwrap();
        assert!(
            buffer
                .stop(false, Deadline::from_millis(1000))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn failure_preserves_decodable_marked_evidence_and_discloses_the_shortened_tail() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let counter = Arc::clone(&calls);
        let failed = Arc::clone(&fail);
        let mut buffer = start(
            options(),
            Deadline::from_millis(3000),
            Box::new(move || {
                Ok(Box::new(move |_| {
                    if failed.load(Ordering::Acquire) {
                        return Err(error("owned surface disappeared"));
                    }
                    counter.fetch_add(1, Ordering::AcqRel);
                    glass_core::Frame::new(64, 48, [20, 90, 120, 255].repeat(64 * 48))
                }))
            }),
        )
        .unwrap();
        until(|| calls.load(Ordering::Acquire) > 1);
        buffer
            .mark(&"b".repeat(32), 5000, Deadline::from_millis(1000))
            .unwrap();
        fail.store(true, Ordering::Release);
        until(|| buffer.progress().state == RollingState::Failed);
        let saved = buffer.save(Deadline::from_millis(1000)).unwrap();
        assert_eq!(saved.video.progress.state, RecordingState::Failed);
        assert!(saved.window.truncated);
        assert!(
            saved
                .video
                .progress
                .reason
                .unwrap()
                .contains("owned surface disappeared")
        );
        glass_core::recording::validate_video(&saved.video.data).unwrap();
        buffer.stop(false, Deadline::from_millis(1000)).unwrap();
    }
}
