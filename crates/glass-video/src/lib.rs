//! Bounded desktop video encoding, separate from application input dispatch.

mod history;
mod movie;
mod rolling;
pub use rolling::start as start_buffer;

/// A capture connection constructed and used on its owning worker thread.
pub type CaptureSource =
    Box<dyn FnMut(glass_core::Deadline) -> glass_core::Result<glass_core::Frame>>;
/// Creates a capture connection without moving native objects across threads.
pub type CaptureFactory = Box<dyn FnOnce() -> glass_core::Result<CaptureSource> + Send>;

use glass_core::{
    Deadline, Frame, GlassError, RecordedVideo, Recording, RecordingFormat, RecordingOptions,
    RecordingState, Result,
    recording_worker::{RecordingControl, start_recording_worker},
};
use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 15);
const CAPTURE_BUDGET: Duration = Duration::from_secs(2);

/// Start an independent capture/encode worker. `setup` creates its private capture connection.
///
/// Each capture must honor its deadline. Sources must stay pinned to their authorized surface;
/// an unavailable target is an error, never permission to switch displays or return stale pixels.
pub fn start<S>(
    options: RecordingOptions,
    deadline: Deadline,
    setup: impl FnOnce() -> Result<S> + Send + 'static,
) -> Result<Box<dyn Recording>>
where
    S: FnMut(Deadline) -> Result<Frame>,
{
    start_recording_worker(options, deadline, move |control| {
        if control.cancelled() {
            return Err(GlassError::Backend("recording cancelled".into()));
        }
        let source = setup()?;
        record(source, control)
    })
}

fn record(
    mut source: impl FnMut(Deadline) -> Result<Frame>,
    control: RecordingControl,
) -> Result<RecordedVideo> {
    let first = source(Deadline::at(Instant::now() + CAPTURE_BUDGET))?;
    let origin = Instant::now();
    let mut movie = movie::Movie::new(&first, control.options().max_bytes)?;
    movie.push(first, 0)?;
    control.observe_bytes(movie.bytes());
    control.ready();
    let mut next = Instant::now() + FRAME_INTERVAL;
    loop {
        if control.should_stop() {
            break;
        }
        let remaining = next.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
            continue;
        }
        capture(&mut source, &mut movie, &control, origin)?;
        next = Instant::now() + FRAME_INTERVAL;
    }
    if control.cancelled() {
        return Err(GlassError::Backend("recording cancelled".into()));
    }
    // A final acquisition includes the verified result even when stop arrives between ticks.
    if control.keep() && control.progress().state == RecordingState::Recording {
        capture(&mut source, &mut movie, &control, origin)?;
    }
    let end_ms = origin.elapsed().as_millis() as u64;
    let data = if control.keep() {
        movie.finish(end_ms)?
    } else {
        Vec::new()
    };
    if !data.is_empty() {
        control.observe_bytes(data.len() as u64);
    }
    Ok(RecordedVideo {
        format: RecordingFormat::Mp4,
        progress: control.mark_finalized(),
        data,
    })
}

fn capture(
    source: &mut impl FnMut(Deadline) -> Result<Frame>,
    movie: &mut movie::Movie,
    control: &RecordingControl,
    origin: Instant,
) -> Result<()> {
    match source(Deadline::at(Instant::now() + CAPTURE_BUDGET)) {
        Ok(frame) => {
            if !movie.push(frame, origin.elapsed().as_millis() as u64)? {
                control.limit("byte_limit");
            }
            control.observe_bytes(movie.bytes());
        }
        Err(error) => control.fail(&format!("desktop video capture failed: {error}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn options() -> RecordingOptions {
        RecordingOptions {
            id: "123456789012345678901234567890ab".into(),
            max_duration_ms: 1000,
            max_bytes: glass_core::recording::RECORDING_MAX_BYTES,
        }
    }

    #[test]
    fn a_capture_failure_reports_failure_and_preserves_valid_video() {
        let mut handle = start(options(), Deadline::from_millis(3000), || {
            let mut first = true;
            Ok(move |_| {
                if std::mem::take(&mut first) {
                    Frame::new(64, 48, [80, 90, 100, 255].repeat(64 * 48))
                } else {
                    Err(GlassError::CaptureFailed(
                        "owned display disconnected".into(),
                    ))
                }
            })
        })
        .unwrap();
        let video = handle.finish(true, Deadline::from_millis(3000)).unwrap();
        assert_eq!(video.progress.state, RecordingState::Failed);
        assert!(
            video
                .progress
                .reason
                .unwrap()
                .contains("owned display disconnected")
        );
        glass_core::recording::validate_video(&video.data).unwrap();
    }

    #[test]
    fn discard_releases_the_owned_source_without_a_movie() {
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        let mut handle = start(options(), Deadline::from_millis(3000), move || {
            let guard = Guard(flag);
            Ok(move |_| {
                let _ = &guard;
                Frame::new(64, 48, [80, 90, 100, 255].repeat(64 * 48))
            })
        })
        .unwrap();
        let video = handle.finish(false, Deadline::from_millis(3000)).unwrap();
        assert!(video.data.is_empty());
        assert!(dropped.load(Ordering::Acquire));
    }
}
