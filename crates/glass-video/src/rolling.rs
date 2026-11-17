use crate::{
    CAPTURE_BUDGET, CaptureFactory, FRAME_INTERVAL,
    history::History,
    movie::{FrameEncoder, Sample},
};
use glass_core::{
    BufferedVideo, Deadline, GlassError, RecordedVideo, RecordingFormat, RecordingProgress,
    RecordingState, Result, RollingBuffer, RollingOptions, RollingProgress, RollingState,
    RollingWindow,
};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type Mux = dyn FnMut(&FrameEncoder, &[Sample], u64, u64) -> Result<Vec<u8>> + Send;

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

struct State {
    history: History,
    history_ms: u64,
    origin: Option<Instant>,
    ended_ms: Option<u64>,
    failure: Option<String>,
    stopped: bool,
}

impl State {
    fn progress(&self) -> RollingProgress {
        let now = self.ended_ms.unwrap_or_else(|| {
            self.origin
                .map_or(0, |origin| origin.elapsed().as_millis() as u64)
        });
        RollingProgress {
            state: if self.stopped {
                RollingState::Stopped
            } else if self.failure.is_some() {
                RollingState::Failed
            } else {
                RollingState::Buffering
            },
            elapsed_ms: now,
            retained_ms: self.history.retained_ms(now),
            bytes: self.history.bytes(),
            pending_bytes: self.history.pending_bytes(),
            truncated: self.history.retained_ms(now) < self.history_ms,
            reason: self.failure.clone(),
            pending: self.history.pending(now),
        }
    }
}

struct Shared {
    state: Mutex<State>,
    cancelled: AtomicBool,
}

impl Shared {
    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| error("buffer state unavailable"))
    }
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
    Ok(Box::new(start_with_mux(
        options,
        deadline,
        setup,
        Box::new(FrameEncoder::mux),
    )?))
}

fn start_with_mux(
    options: RollingOptions,
    deadline: Deadline,
    setup: CaptureFactory,
    mut mux: Box<Mux>,
) -> Result<Handle> {
    options.validate()?;
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started("rolling history startup"));
    }
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            history: History::new(options.clone()),
            history_ms: options.history_ms,
            origin: None,
            ended_ms: None,
            failure: None,
            stopped: false,
        }),
        cancelled: AtomicBool::new(false),
    });
    let (ready, started) = mpsc::sync_channel(1);
    let (commands, incoming) = mpsc::sync_channel(1);
    let background = Arc::clone(&shared);
    let thread = std::thread::Builder::new()
        .name("glass-video-history".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    options,
                    setup,
                    &background,
                    incoming,
                    ready.clone(),
                    &mut *mux,
                )
            }))
            .unwrap_or_else(|_| Err(error("capture worker panicked")));
            if let Err(failure) = result {
                if let Ok(mut state) = background.state() {
                    state.ended_ms = Some(state.progress().elapsed_ms);
                    state.history.discard();
                    state.failure = Some(failure.to_string());
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
        Ok(Ok(())) => Ok(handle),
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
        self.shared
            .state()
            .map(|state| state.progress())
            .unwrap_or_else(|failure| RollingProgress {
                state: RollingState::Failed,
                elapsed_ms: 0,
                retained_ms: 0,
                bytes: 0,
                pending_bytes: 0,
                truncated: true,
                reason: Some(failure.to_string()),
                pending: None,
            })
    }
    fn mark(&mut self, clip_id: &str, tail_ms: u64, deadline: Deadline) -> Result<RollingWindow> {
        // Rendezvous delivery distinguishes a received result from an abandoned reply.
        let (reply, result) = mpsc::sync_channel(0);
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
        let (reply, result) = mpsc::sync_channel(0);
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
    window: RollingWindow,
    samples: &[Sample],
    failed: bool,
    package: impl FnOnce(&[Sample], u64) -> Result<Vec<u8>>,
) -> Result<BufferedVideo> {
    let data = package(samples, window.end_ms)?;
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
    mux: &mut Mux,
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
    let initial = encoder.encode(first, 0)?;
    if !initial.sync {
        return Err(error("encoder produced no initial keyframe"));
    }
    {
        let mut state = shared.state()?;
        state.history.push(initial)?;
        state.origin = Some(origin);
    }
    let mut saving: Option<(Deadline, mpsc::SyncSender<Result<BufferedVideo>>)> = None;
    let mut next = Instant::now() + FRAME_INTERVAL;
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
            } else if shared
                .state()?
                .history
                .pending(now)
                .is_some_and(|p| p.ready)
                && let Some((_, reply)) = saving.take()
            {
                let (selected, failed) = {
                    let state = shared.state()?;
                    (state.history.snapshot(now, false), state.failure.is_some())
                };
                let result = selected.and_then(|(window, samples)| {
                    export(window, &samples, failed, |samples, end| {
                        mux(&encoder, samples, end, options.max_bytes)
                    })
                });
                let successful = result.is_ok();
                let mut state = shared.state()?;
                if reply.send(result).is_ok() && successful {
                    state.history.discard();
                }
            }
        }
        match commands.recv_timeout(Duration::from_millis(5)) {
            Ok(Command::Mark {
                id,
                tail_ms,
                deadline,
                reply,
            }) => {
                let mut state = shared.state()?;
                let previous = state.history.clone();
                let result = if deadline.has_passed() || saving.is_some() {
                    Err(error("mark deadline elapsed or a save is in progress"))
                } else {
                    let (marked_at, tail_ms) = if state.failure.is_some() {
                        (state.history.latest_ms() + 1, 0)
                    } else {
                        (now, tail_ms)
                    };
                    state.history.mark(&id, marked_at, tail_ms).map(|window| {
                        if let Some(reason) = state.failure.clone() {
                            state.history.fail(now, &reason);
                        }
                        state.history.pending(now).unwrap_or(window)
                    })
                };
                if reply.send(result).is_err() {
                    state.history = previous;
                }
            }
            Ok(Command::Save { deadline, reply }) => {
                if saving.is_some() || shared.state()?.history.pending(now).is_none() {
                    let _ = reply.send(Err(error(
                        "no marked clip awaits saving, or another save is in progress",
                    )));
                } else {
                    saving = Some((deadline, reply));
                }
            }
            Ok(Command::Stop { keep, reply }) => {
                let (selected, failed) = {
                    let state = shared.state()?;
                    let selected = if keep && state.history.pending(now).is_some() {
                        Some(state.history.snapshot(now, true))
                    } else {
                        None
                    };
                    (selected, state.failure.is_some())
                };
                let result = selected
                    .map(|selected| {
                        let (window, samples) = selected?;
                        export(window, &samples, failed, |samples, end| {
                            mux(&encoder, samples, end, options.max_bytes)
                        })
                    })
                    .transpose();
                source.take();
                {
                    let mut state = shared.state()?;
                    state.history.discard();
                    state.stopped = true;
                    state.ended_ms = Some(now);
                }
                let _ = reply.send(result);
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next
            && let Some(capture) = source.as_mut()
        {
            let capture_at = origin.elapsed().as_millis() as u64;
            let captured =
                capture(Deadline::at(Instant::now() + CAPTURE_BUDGET)).and_then(|frame| {
                    let now = origin.elapsed().as_millis() as u64;
                    if shared.state()?.history.keyframe_due(now) {
                        encoder.force_keyframe();
                    }
                    let sample = encoder.encode(frame, now)?;
                    shared.state()?.history.push(sample)
                });
            if let Err(failure) = captured {
                let reason = format!("buffer capture failed: {failure}");
                {
                    let mut state = shared.state()?;
                    state.history.fail(capture_at, &reason);
                    state.failure = Some(reason);
                }
                source.take();
            }
            next = Instant::now() + FRAME_INTERVAL;
        }
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
    fn a_lost_first_mark_reply_does_not_leave_an_orphaned_clip() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut buffer = start_with_mux(
            options(),
            Deadline::from_millis(3000),
            source(Arc::clone(&calls)),
            Box::new(FrameEncoder::mux),
        )
        .unwrap();
        let before = calls.load(Ordering::Acquire);
        let (reply, abandoned) = mpsc::sync_channel(0);
        buffer
            .send(
                Command::Mark {
                    id: "b".repeat(32),
                    tail_ms: 0,
                    deadline: Deadline::from_millis(1000),
                    reply,
                },
                Deadline::from_millis(1000),
            )
            .unwrap();
        drop(abandoned);
        until(|| calls.load(Ordering::Acquire) >= before + 2);
        assert!(buffer.progress().pending.is_none());
        let marked = buffer
            .mark(&"c".repeat(32), 0, Deadline::from_millis(1000))
            .unwrap();
        let saved = buffer.save(Deadline::from_millis(1000)).unwrap();
        assert_eq!(saved.window.id, marked.id);
        buffer.stop(false, Deadline::from_millis(1000)).unwrap();
    }

    #[test]
    fn a_lost_merged_mark_reply_preserves_the_acknowledged_selection() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut buffer = start_with_mux(
            options(),
            Deadline::from_millis(3000),
            source(Arc::clone(&calls)),
            Box::new(FrameEncoder::mux),
        )
        .unwrap();
        let marked = buffer
            .mark(&"b".repeat(32), 0, Deadline::from_millis(1000))
            .unwrap();
        let before = calls.load(Ordering::Acquire);
        let (reply, abandoned) = mpsc::sync_channel(0);
        buffer
            .send(
                Command::Mark {
                    id: marked.id.clone(),
                    tail_ms: 5000,
                    deadline: Deadline::from_millis(1000),
                    reply,
                },
                Deadline::from_millis(1000),
            )
            .unwrap();
        drop(abandoned);
        until(|| calls.load(Ordering::Acquire) >= before + 2);
        let pending = buffer.progress().pending.unwrap();
        assert_eq!(
            (pending.start_ms, pending.end_ms, pending.marked_at_ms),
            (marked.start_ms, marked.end_ms, marked.marked_at_ms)
        );
        let saved = buffer.save(Deadline::from_millis(1000)).unwrap();
        assert_eq!(saved.window.id, marked.id);
        assert_eq!(saved.window.end_ms, marked.end_ms);
        buffer.stop(false, Deadline::from_millis(1000)).unwrap();
    }

    #[test]
    fn an_unread_export_reply_preserves_the_selected_clip() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (packaged, packaging) = mpsc::sync_channel(1);
        let mut buffer = start_with_mux(
            options(),
            Deadline::from_millis(3000),
            source(Arc::clone(&calls)),
            Box::new(move |encoder, samples, end, max| {
                let data = encoder.mux(samples, end, max)?;
                let _ = packaged.try_send(());
                Ok(data)
            }),
        )
        .unwrap();
        let marked = buffer
            .mark(&"b".repeat(32), 0, Deadline::from_millis(1000))
            .unwrap();
        let (reply, abandoned) = mpsc::sync_channel(0);
        buffer
            .send(
                Command::Save {
                    deadline: Deadline::from_millis(1000),
                    reply,
                },
                Deadline::from_millis(1000),
            )
            .unwrap();
        packaging.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(abandoned);
        let before = calls.load(Ordering::Acquire);
        until(|| calls.load(Ordering::Acquire) >= before + 2);
        assert_eq!(buffer.progress().pending.unwrap().id, marked.id);
        let saved = buffer.save(Deadline::from_millis(1000)).unwrap();
        assert_eq!(
            (saved.window.id, saved.window.start_ms, saved.window.end_ms),
            (marked.id, marked.start_ms, marked.end_ms)
        );
        buffer.stop(false, Deadline::from_millis(1000)).unwrap();
    }

    #[test]
    fn a_failed_export_can_be_kept_by_stop_or_explicitly_discarded() {
        for keep in [false, true] {
            let mut first = true;
            let mut buffer = start_with_mux(
                options(),
                Deadline::from_millis(3000),
                source(Arc::new(AtomicUsize::new(0))),
                Box::new(move |encoder, samples, end, max| {
                    if std::mem::take(&mut first) {
                        Err(error("injected packaging failure"))
                    } else {
                        encoder.mux(samples, end, max)
                    }
                }),
            )
            .unwrap();
            let marked = buffer
                .mark(&"b".repeat(32), 0, Deadline::from_millis(1000))
                .unwrap();
            assert!(buffer.save(Deadline::from_millis(1000)).is_err());
            let stopped = buffer.stop(keep, Deadline::from_millis(1000)).unwrap();
            assert_eq!(stopped.is_some(), keep);
            if let Some(saved) = stopped {
                assert_eq!(saved.window.id, marked.id);
                glass_core::recording::validate_video(&saved.video.data).unwrap();
            }
            let final_progress = buffer.progress();
            assert_eq!(final_progress.state, RollingState::Stopped);
            assert!(final_progress.pending.is_none());
            std::thread::sleep(Duration::from_millis(5));
            assert_eq!(buffer.progress().elapsed_ms, final_progress.elapsed_ms);
        }
    }

    #[test]
    fn packaging_failure_keeps_the_selected_clip_for_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut first = true;
        let mut buffer = start_with_mux(
            options(),
            Deadline::from_millis(3000),
            source(Arc::clone(&calls)),
            Box::new(move |encoder, samples, end, max| {
                if std::mem::take(&mut first) {
                    Err(error("injected packaging failure"))
                } else {
                    encoder.mux(samples, end, max)
                }
            }),
        )
        .unwrap();
        let marked = buffer
            .mark(&"b".repeat(32), 0, Deadline::from_millis(1000))
            .unwrap();
        let failure = buffer.save(Deadline::from_millis(1000)).err().unwrap();
        assert!(failure.to_string().contains("injected packaging failure"));
        let pending = buffer.progress().pending.unwrap();
        assert_eq!(pending.id, marked.id);
        assert_eq!(
            (pending.start_ms, pending.end_ms),
            (marked.start_ms, marked.end_ms)
        );
        let before = calls.load(Ordering::Acquire);
        until(|| calls.load(Ordering::Acquire) > before);
        let saved = buffer.save(Deadline::from_millis(1000)).unwrap();
        assert_eq!(saved.window.id, marked.id);
        assert_eq!(
            (saved.window.start_ms, saved.window.end_ms),
            (marked.start_ms, marked.end_ms)
        );
        glass_core::recording::validate_video(&saved.video.data).unwrap();
        assert!(buffer.progress().pending.is_none());
        buffer.stop(false, Deadline::from_millis(1000)).unwrap();
    }

    #[test]
    fn a_timeout_during_packaging_keeps_the_selected_clip_for_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered, packaging) = mpsc::sync_channel(1);
        let (release, proceed) = mpsc::sync_channel(1);
        let mut first = true;
        let mut buffer = start_with_mux(
            options(),
            Deadline::from_millis(3000),
            source(Arc::clone(&calls)),
            Box::new(move |encoder, samples, end, max| {
                if std::mem::take(&mut first) {
                    let _ = entered.send(());
                    proceed
                        .recv_timeout(Duration::from_secs(3))
                        .map_err(error)?;
                }
                encoder.mux(samples, end, max)
            }),
        )
        .unwrap();
        let marked = buffer
            .mark(&"b".repeat(32), 0, Deadline::from_millis(1000))
            .unwrap();
        let saving = std::thread::spawn(move || {
            let result = buffer.save(Deadline::from_millis(200));
            (buffer, result)
        });
        packaging.recv_timeout(Duration::from_secs(3)).unwrap();
        let (mut buffer, result) = saving.join().unwrap();
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("deadline elapsed")
        );
        let before = calls.load(Ordering::Acquire);
        release.send(()).unwrap();
        until(|| calls.load(Ordering::Acquire) > before);
        let pending = buffer.progress().pending.unwrap();
        assert_eq!(pending.id, marked.id);
        assert_eq!(
            (pending.start_ms, pending.end_ms),
            (marked.start_ms, marked.end_ms)
        );
        let saved = buffer.save(Deadline::from_millis(1000)).unwrap();
        assert_eq!(saved.window.id, marked.id);
        glass_core::recording::validate_video(&saved.video.data).unwrap();
        buffer.stop(false, Deadline::from_millis(1000)).unwrap();
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
    fn a_capture_that_fails_after_spending_the_tail_budget_still_reports_truncation() {
        let fail = Arc::new(AtomicBool::new(false));
        let failed = Arc::clone(&fail);
        let (entered, capturing) = mpsc::sync_channel(1);
        let (release, proceed) = mpsc::sync_channel(1);
        let mut buffer = start(
            options(),
            Deadline::from_millis(3000),
            Box::new(move || {
                Ok(Box::new(move |_| {
                    if failed.load(Ordering::Acquire) {
                        let _ = entered.send(());
                        proceed
                            .recv_timeout(Duration::from_secs(3))
                            .map_err(error)?;
                        return Err(error("display disconnected"));
                    }
                    glass_core::Frame::new(64, 48, [20, 90, 120, 255].repeat(64 * 48))
                }))
            }),
        )
        .unwrap();
        let marked = buffer
            .mark(&"b".repeat(32), 1000, Deadline::from_millis(1000))
            .unwrap();
        fail.store(true, Ordering::Release);
        capturing.recv_timeout(Duration::from_secs(3)).unwrap();
        until(|| buffer.progress().elapsed_ms > marked.end_ms);
        release.send(()).unwrap();
        until(|| buffer.progress().state == RollingState::Failed);
        let saved = buffer.save(Deadline::from_millis(1000)).unwrap();
        assert!(saved.window.end_ms < marked.end_ms);
        assert!(saved.window.truncated);
        assert_eq!(saved.video.progress.state, RecordingState::Failed);
        glass_core::recording::validate_video(&saved.video.data).unwrap();
        buffer.stop(false, Deadline::from_millis(1000)).unwrap();
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
