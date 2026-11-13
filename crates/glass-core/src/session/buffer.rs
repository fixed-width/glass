use super::recording::FinishedRecording;
use super::*;
use crate::recording::RECORDING_MAX_BYTES;
use crate::rolling::*;
use crate::{
    BufferedVideo, RecordingMark, RecordingOutcome, RecordingProgress, RecordingState,
    RecordingStatus, RecordingSummary,
};

fn error(reason: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(reason.to_string())
}

fn progress(window: &RollingWindow, state: RollingState) -> RecordingProgress {
    RecordingProgress {
        state: if state == RollingState::Failed {
            RecordingState::Failed
        } else if window.reason.is_some() {
            RecordingState::Limited
        } else if window.ready {
            RecordingState::Finalized
        } else {
            RecordingState::Recording
        },
        elapsed_ms: window.end_ms.saturating_sub(window.start_ms),
        bytes: window.bytes,
        reason: window.reason.clone(),
    }
}

fn outcome_rank(outcome: RecordingOutcome) -> u8 {
    match outcome {
        RecordingOutcome::Failure => 3,
        RecordingOutcome::Cancelled => 2,
        RecordingOutcome::Interrupted => 1,
        RecordingOutcome::Success => 0,
    }
}

impl Glass {
    pub(super) fn rolling_summary(&self) -> Option<RollingSummary> {
        self.recordings.buffer.as_ref().map(|buffer| {
            let mut summary = buffer.summary.clone();
            summary.progress = buffer.handle.progress();
            summary.pending = buffer.pending.clone().map(|mut pending| {
                if let Some(window) = &summary.progress.pending {
                    pending.progress = progress(window, summary.progress.state);
                    pending.window = Some(window.clone());
                }
                pending
            });
            summary
        })
    }

    pub fn start_rolling_buffer(&mut self, history_ms: Option<u64>) -> Result<RollingSummary> {
        let sink =
            self.recordings
                .sink
                .as_ref()
                .ok_or_else(|| GlassError::UnsupportedOperation {
                    operation: "rolling video history".into(),
                    reason: "start the server with --record-dir to enable video evidence".into(),
                })?;
        if self.recordings.active.is_some() || self.recordings.buffer.is_some() {
            return Err(error(
                "a capture is already active; stop it before starting a rolling buffer",
            ));
        }
        let session = self.active.as_ref().ok_or(GlassError::NoActiveSession)?;
        if !session.platform.rolling_buffer_supported() {
            return Err(GlassError::UnsupportedOperation {
                operation: "rolling video history".into(),
                reason: "this backend has no encoded rolling-buffer adapter".into(),
            });
        }
        let options = RollingOptions {
            id: sink.next_id()?,
            history_ms: history_ms.unwrap_or(BUFFER_DEFAULT_HISTORY_MS),
            max_bytes: RECORDING_MAX_BYTES,
        };
        options.validate()?;
        let id = options.id.clone();
        let history_ms = options.history_ms;
        let session = self.active_mut()?;
        let handle = session
            .platform
            .start_rolling_buffer(options, Deadline::from_millis(15_000))?;
        let summary = RollingSummary {
            id,
            backend: session.backend.clone(),
            scope: session.platform.recording_scope().as_str().into(),
            history_ms,
            progress: handle.progress(),
            pending: None,
        };
        self.recordings.buffer = Some(ActiveBuffer {
            summary: summary.clone(),
            handle,
            pending: None,
        });
        Ok(summary)
    }

    pub fn mark_rolling_buffer(
        &mut self,
        name: &str,
        outcome: RecordingOutcome,
        keep: Option<bool>,
        tail_ms: Option<u64>,
    ) -> Result<Option<RecordingSummary>> {
        if name.trim().is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
            return Err(error(
                "recording name must be nonblank, at most 120 bytes, without control characters",
            ));
        }
        let tail_ms = tail_ms.unwrap_or(BUFFER_DEFAULT_TAIL_MS);
        if tail_ms > BUFFER_MAX_TAIL_MS {
            return Err(error("buffer tail must be between 0 and 5000 ms"));
        }
        let buffer = self
            .recordings
            .buffer
            .as_ref()
            .ok_or_else(|| error("no rolling buffer is active"))?;
        if !keep.unwrap_or(outcome != RecordingOutcome::Success) {
            return Ok(None);
        }
        if buffer
            .pending
            .as_ref()
            .is_some_and(|p| p.marks.len() >= BUFFER_MAX_MARKS)
        {
            return Err(error(
                "pending clip has 32 marks; save it before another mark",
            ));
        }
        let id = match &buffer.pending {
            Some(pending) => pending.id.clone(),
            None => self
                .recordings
                .sink
                .as_ref()
                .ok_or_else(|| error("recording storage unavailable"))?
                .next_id()?,
        };
        let buffer = self
            .recordings
            .buffer
            .as_mut()
            .ok_or_else(|| error("no rolling buffer is active"))?;
        let window = buffer
            .handle
            .mark(&id, tail_ms, Deadline::from_millis(5000))?;
        let mut pending = buffer.pending.clone().unwrap_or_else(|| RecordingSummary {
            id,
            name: name.into(),
            backend: buffer.summary.backend.clone(),
            scope: buffer.summary.scope.clone(),
            progress: progress(&window, RollingState::Buffering),
            outcome: Some(outcome),
            kept: false,
            artifact: None,
            marks: Vec::new(),
            window: None,
        });
        pending.outcome = Some(pending.outcome.map_or(outcome, |previous| {
            std::cmp::max_by_key(previous, outcome, |o| outcome_rank(*o))
        }));
        pending.marks.push(RecordingMark {
            name: name.into(),
            outcome,
            at_ms: window.marked_at_ms,
        });
        pending.progress = progress(&window, buffer.handle.progress().state);
        pending.window = Some(window);
        buffer.pending = Some(pending.clone());
        Ok(Some(pending))
    }

    fn finished_buffered(
        &mut self,
        mut summary: RecordingSummary,
        captured: BufferedVideo,
    ) -> Result<FinishedRecording> {
        if captured.window.id != summary.id {
            let failure = error("buffer clip identity mismatch");
            summary.progress.state = RecordingState::Failed;
            summary.progress.reason = Some(failure.to_string());
            self.recordings.last = Some(summary);
            return Err(failure);
        }
        summary.progress = captured.video.progress.clone();
        summary.window = Some(captured.window);
        Ok(FinishedRecording {
            summary,
            video: Some(captured.video),
        })
    }

    pub fn save_rolling_buffer(&mut self, deadline: Deadline) -> Result<RecordingSummary> {
        let buffer = self
            .recordings
            .buffer
            .as_mut()
            .ok_or_else(|| error("no rolling buffer is active"))?;
        let summary = buffer
            .pending
            .as_ref()
            .ok_or_else(|| error("no marked clip awaits saving"))?
            .clone();
        match buffer.handle.save(deadline) {
            Ok(captured) => {
                buffer.pending = None;
                let finished = self.finished_buffered(summary, captured)?;
                self.persist_recording(finished)
            }
            Err(failure) => {
                let mut failed = summary;
                failed.progress.state = RecordingState::Failed;
                failed.progress.reason = Some(failure.to_string());
                self.recordings.last = Some(failed);
                Err(failure)
            }
        }
    }

    pub fn stop_rolling_buffer(
        &mut self,
        keep: Option<bool>,
        deadline: Deadline,
    ) -> Result<RecordingStatus> {
        if let Some(finished) = self.finalize_rolling_buffer(keep, deadline)? {
            self.persist_recording(finished)?;
        }
        Ok(self.recording_status())
    }

    pub(super) fn finalize_rolling_buffer(
        &mut self,
        keep: Option<bool>,
        deadline: Deadline,
    ) -> Result<Option<FinishedRecording>> {
        let mut buffer = self
            .recordings
            .buffer
            .take()
            .ok_or_else(|| error("no rolling buffer is active"))?;
        let keep = keep.unwrap_or(true);
        let captured = buffer.handle.stop(keep, deadline);
        match (captured, buffer.pending) {
            (Ok(Some(video)), Some(summary)) => self.finished_buffered(summary, video).map(Some),
            (Ok(None), Some(mut summary)) if !keep => {
                summary.progress.state = RecordingState::Discarded;
                Ok(Some(FinishedRecording {
                    summary,
                    video: None,
                }))
            }
            (Ok(None), None) => Ok(None),
            (Err(failure), pending) => {
                if let Some(mut summary) = pending {
                    summary.progress.state = RecordingState::Failed;
                    summary.progress.reason = Some(failure.to_string());
                    self.recordings.last = Some(summary);
                }
                Err(failure)
            }
            _ => Err(error(
                "native buffer disposition did not match the pending clip",
            )),
        }
    }
}

#[cfg(test)]
#[path = "buffer_tests.rs"]
mod tests;
