//! Shared control for an independently owned native recorder worker.

use crate::{
    Deadline, GlassError, RecordedVideo, Recording, RecordingOptions, RecordingProgress,
    RecordingState, Result,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Shared {
    options: RecordingOptions,
    stop: AtomicBool,
    cancel: AtomicBool,
    keep: AtomicBool,
    started: Mutex<Option<Instant>>,
    progress: Mutex<RecordingProgress>,
    ready: Mutex<Option<mpsc::Sender<std::result::Result<(), String>>>>,
}

#[derive(Clone)]
pub struct RecordingControl(Arc<Shared>);

impl RecordingControl {
    /// Preserve a failed capture's valid prefix for explicit disposition.
    pub fn fail(&self, reason: &str) {
        if let Ok(mut progress) = self.0.progress.lock() {
            progress.state = RecordingState::Failed;
            progress.reason = Some(reason.to_owned());
            progress.elapsed_ms = self.elapsed_ms();
        }
        self.0.stop.store(true, Ordering::Release);
    }
    pub fn limit(&self, reason: &str) {
        if let Ok(mut progress) = self.0.progress.lock() {
            progress.state = RecordingState::Limited;
            progress.reason = Some(reason.to_owned());
        }
        self.0.stop.store(true, Ordering::Release);
    }
    pub fn options(&self) -> &RecordingOptions {
        &self.0.options
    }
    pub fn cancelled(&self) -> bool {
        self.0.cancel.load(Ordering::Acquire)
    }
    pub fn keep(&self) -> bool {
        self.0.keep.load(Ordering::Acquire)
    }
    pub fn elapsed_ms(&self) -> u64 {
        self.0
            .started
            .lock()
            .ok()
            .and_then(|start| *start)
            .map_or(0, |start| {
                start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
            })
    }
    pub fn ready(&self) {
        if let Ok(mut start) = self.0.started.lock()
            && start.is_none()
        {
            *start = Some(Instant::now());
        }
        if let Ok(mut sender) = self.0.ready.lock()
            && let Some(sender) = sender.take()
        {
            let _ = sender.send(Ok(()));
        }
    }
    pub fn observe_bytes(&self, bytes: u64) {
        if let Ok(mut progress) = self.0.progress.lock() {
            progress.bytes = bytes;
            if bytes >= self.0.options.max_bytes.saturating_sub(256 * 1024) {
                progress.state = RecordingState::Limited;
                progress.reason = Some("byte_limit".into());
                self.0.stop.store(true, Ordering::Release);
            }
        }
    }
    pub fn should_stop(&self) -> bool {
        if self.elapsed_ms() >= self.0.options.max_duration_ms {
            if let Ok(mut progress) = self.0.progress.lock() {
                progress.state = RecordingState::Limited;
                progress.reason = Some("duration_limit".into());
            }
            self.0.stop.store(true, Ordering::Release);
        }
        self.cancelled() || self.0.stop.load(Ordering::Acquire)
    }
    pub fn progress(&self) -> RecordingProgress {
        let mut progress = self
            .0
            .progress
            .lock()
            .map(|p| p.clone())
            .unwrap_or(RecordingProgress {
                state: RecordingState::Failed,
                elapsed_ms: 0,
                bytes: 0,
                reason: Some("recording state unavailable".into()),
            });
        if progress.state == RecordingState::Recording {
            progress.elapsed_ms = self.elapsed_ms();
        }
        progress
    }
    pub fn mark_finalized(&self) -> RecordingProgress {
        if let Ok(mut progress) = self.0.progress.lock() {
            progress.elapsed_ms = self.elapsed_ms();
            if progress.state == RecordingState::Recording {
                progress.state = RecordingState::Finalized;
            }
        }
        self.progress()
    }
}

struct Worker {
    control: RecordingControl,
    result: mpsc::Receiver<Result<RecordedVideo>>,
    thread: Option<JoinHandle<()>>,
    finished: bool,
}

/// Start capture and acknowledge actual video data before returning its handle.
pub fn start_recording_worker(
    options: RecordingOptions,
    deadline: Deadline,
    work: impl FnOnce(RecordingControl) -> Result<RecordedVideo> + Send + 'static,
) -> Result<Box<dyn Recording>> {
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started("recording startup"));
    }
    options.validate()?;
    let (ready_tx, ready_rx) = mpsc::channel();
    let (result_tx, result) = mpsc::channel();
    let control = RecordingControl(Arc::new(Shared {
        options,
        stop: AtomicBool::new(false),
        cancel: AtomicBool::new(false),
        keep: AtomicBool::new(true),
        started: Mutex::new(None),
        progress: Mutex::new(RecordingProgress {
            state: RecordingState::Recording,
            elapsed_ms: 0,
            bytes: 0,
            reason: None,
        }),
        ready: Mutex::new(Some(ready_tx)),
    }));
    let background = control.clone();
    let thread = std::thread::Builder::new()
        .name("glass-recording".into())
        .spawn(move || {
            let mut completed =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(background.clone())))
                    .unwrap_or_else(|_| {
                        Err(GlassError::Backend(
                            "native recorder worker panicked".into(),
                        ))
                    });
            if background
                .0
                .started
                .lock()
                .ok()
                .is_some_and(|start| start.is_none())
                && completed.is_ok()
            {
                completed = Err(GlassError::Backend(
                    "recorder ended before acknowledging video data".into(),
                ));
            }
            if let Err(error) = &completed {
                if let Ok(mut state) = background.0.progress.lock() {
                    state.state = RecordingState::Failed;
                    state.reason = Some(error.to_string());
                    state.elapsed_ms = background.elapsed_ms();
                }
                if let Ok(mut sender) = background.0.ready.lock()
                    && let Some(sender) = sender.take()
                {
                    let _ = sender.send(Err(error.to_string()));
                }
            }
            let _ = result_tx.send(completed);
        })
        .map_err(|e| GlassError::Backend(format!("start recorder worker: {e}")))?;
    let worker = Worker {
        control,
        result,
        thread: Some(thread),
        finished: false,
    };
    let timeout = deadline
        .remaining()
        .unwrap_or(Duration::from_secs(15))
        .min(Duration::from_secs(15));
    match ready_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(Box::new(worker)),
        Ok(Err(error)) => Err(GlassError::Backend(error)),
        Err(_) => Err(GlassError::Backend(
            "recorder did not acknowledge video data before startup deadline".into(),
        )),
    }
}

impl Recording for Worker {
    fn progress(&self) -> RecordingProgress {
        self.control.progress()
    }
    fn finish(&mut self, keep: bool, deadline: Deadline) -> Result<RecordedVideo> {
        if self.finished {
            return Err(GlassError::Backend("recording already finalized".into()));
        }
        self.control.0.keep.store(keep, Ordering::Release);
        self.control.0.stop.store(true, Ordering::Release);
        let timeout = deadline
            .remaining()
            .unwrap_or(Duration::from_secs(20))
            .min(Duration::from_secs(20));
        let result = self
            .result
            .recv_timeout(timeout)
            .map_err(|_| GlassError::Backend("recording finalization deadline elapsed".into()))?;
        self.finished = true;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        result
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if !self.finished {
            self.control.0.cancel.store(true, Ordering::Release);
        }
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> RecordingControl {
        RecordingControl(Arc::new(Shared {
            options: options(),
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            keep: AtomicBool::new(true),
            started: Mutex::new(None),
            progress: Mutex::new(RecordingProgress {
                state: RecordingState::Recording,
                elapsed_ms: 0,
                bytes: 0,
                reason: None,
            }),
            ready: Mutex::new(None),
        }))
    }

    #[test]
    fn byte_pressure_stops_before_the_movie_allowance_is_exhausted() {
        let control = control();
        let threshold = control.options().max_bytes - 256 * 1024;
        control.observe_bytes(threshold - 1);
        assert_eq!(control.progress().bytes, threshold - 1);
        assert!(!control.should_stop());
        control.observe_bytes(threshold);
        assert!(control.should_stop());
        let progress = control.mark_finalized();
        assert_eq!(progress.state, RecordingState::Limited);
        assert_eq!(progress.reason.as_deref(), Some("byte_limit"));
    }

    #[test]
    fn elapsed_capture_time_is_live_only_until_a_terminal_state() {
        let control = control();
        assert_eq!(control.elapsed_ms(), 0);
        control.ready();
        *control.0.started.lock().unwrap() = Some(Instant::now() - Duration::from_millis(100));
        assert!((100..1000).contains(&control.progress().elapsed_ms));
        let finalized = control.mark_finalized();
        assert_eq!(finalized.state, RecordingState::Finalized);
        *control.0.started.lock().unwrap() = Some(Instant::now() - Duration::from_millis(150));
        assert_eq!(control.progress().elapsed_ms, finalized.elapsed_ms);
        assert!(!control.should_stop());
    }

    #[test]
    fn failures_and_limits_keep_their_reason_after_finalization() {
        let failed = control();
        failed.ready();
        failed.fail("display disconnected");
        assert!(failed.should_stop());
        let progress = failed.mark_finalized();
        assert_eq!(progress.state, RecordingState::Failed);
        assert_eq!(progress.reason.as_deref(), Some("display disconnected"));
        let limited = control();
        limited.limit("native duration limit");
        assert!(limited.should_stop());
        assert_eq!(limited.mark_finalized().state, RecordingState::Limited);
        assert_eq!(
            limited.progress().reason.as_deref(),
            Some("native duration limit")
        );
    }

    #[test]
    fn duration_limit_and_explicit_keep_or_discard_reach_the_worker() {
        let timed = control();
        timed.ready();
        *timed.0.started.lock().unwrap() = Some(Instant::now() - Duration::from_millis(1000));
        assert!(timed.should_stop());
        assert_eq!(timed.progress().reason.as_deref(), Some("duration_limit"));
        for keep in [false, true] {
            let mut worker =
                start_recording_worker(options(), Deadline::from_millis(1000), |control| {
                    control.ready();
                    while !control.should_stop() {
                        std::thread::yield_now();
                    }
                    Ok(RecordedVideo {
                        format: crate::RecordingFormat::Mp4,
                        progress: control.mark_finalized(),
                        data: if control.keep() { vec![1] } else { vec![] },
                    })
                })
                .unwrap();
            assert_eq!(
                worker
                    .finish(keep, Deadline::from_millis(1000))
                    .unwrap()
                    .data,
                if keep { vec![1] } else { vec![] }
            );
        }
    }
    fn options() -> RecordingOptions {
        RecordingOptions {
            id: "123456789012345678901234567890ab".into(),
            max_duration_ms: 1000,
            max_bytes: 8 * 1024 * 1024,
        }
    }
    #[test]
    fn acknowledges_capture_then_stops_without_serializing_application_work() {
        let mut handle =
            start_recording_worker(options(), Deadline::from_millis(1000), |control| {
                control.observe_bytes(200);
                control.ready();
                while !control.should_stop() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(RecordedVideo {
                    format: crate::RecordingFormat::Mp4,
                    progress: control.mark_finalized(),
                    data: vec![1],
                })
            })
            .unwrap();
        assert_eq!(handle.progress().state, RecordingState::Recording);
        assert_eq!(
            handle
                .finish(true, Deadline::from_millis(1000))
                .unwrap()
                .data,
            vec![1]
        );
    }
    #[test]
    fn startup_timeout_cancels_the_worker_and_spent_deadlines_start_nothing() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let observed = cleaned.clone();
        let started =
            start_recording_worker(options(), Deadline::from_millis(20), move |control| {
                while !control.cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                observed.store(true, Ordering::Release);
                Err(GlassError::Backend("cancelled".into()))
            });
        assert!(started.is_err());
        let deadline = Instant::now() + Duration::from_secs(1);
        while !cleaned.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(cleaned.load(Ordering::Acquire));
        assert!(
            start_recording_worker(options(), Deadline::from_millis(0), |_| panic!(
                "must not start"
            ))
            .is_err()
        );
    }
    #[test]
    fn native_failure_is_reported_before_start_returns() {
        let result = start_recording_worker(options(), Deadline::from_millis(1000), |_| {
            Err(GlassError::Backend("no codec".into()))
        });
        assert!(matches!(result,Err(error) if error.to_string().contains("no codec")));
    }
}
