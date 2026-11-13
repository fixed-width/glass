//! Encoded recent history and explicitly marked incident windows.

use crate::{Deadline, RecordedVideo, RecordingOptions, RecordingSummary, Result};
use serde::{Deserialize, Serialize};

pub const BUFFER_DEFAULT_HISTORY_MS: u64 = 10_000;
pub const BUFFER_MAX_HISTORY_MS: u64 = 30_000;
pub const BUFFER_DEFAULT_TAIL_MS: u64 = 2000;
pub const BUFFER_MAX_TAIL_MS: u64 = 5000;
pub const BUFFER_MAX_MARKS: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollingOptions {
    pub id: String,
    pub history_ms: u64,
    pub max_bytes: u64,
}

impl RollingOptions {
    pub fn validate(&self) -> Result<()> {
        RecordingOptions {
            id: self.id.clone(),
            max_duration_ms: 1000,
            max_bytes: self.max_bytes,
        }
        .validate()?;
        if !(1000..=BUFFER_MAX_HISTORY_MS).contains(&self.history_ms) {
            return Err(crate::GlassError::Backend(
                "buffer history must be between 1000 and 30000 ms".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingState {
    Buffering,
    Failed,
    Stopped,
}

/// Buffer-clock offsets describing one independently decodable marked movie.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollingWindow {
    pub id: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub marked_at_ms: u64,
    pub captured_until_ms: u64,
    pub bytes: u64,
    pub ready: bool,
    pub truncated: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollingProgress {
    pub state: RollingState,
    pub elapsed_ms: u64,
    pub retained_ms: u64,
    pub bytes: u64,
    pub pending_bytes: u64,
    pub truncated: bool,
    pub reason: Option<String>,
    pub pending: Option<RollingWindow>,
}

pub struct BufferedVideo {
    pub window: RollingWindow,
    pub video: RecordedVideo,
}

/// Owns recent history. Dropping a handle cancels capture and discards unexported bytes.
pub trait RollingBuffer: Send {
    fn progress(&self) -> RollingProgress;
    fn mark(&mut self, clip_id: &str, tail_ms: u64, deadline: Deadline) -> Result<RollingWindow>;
    fn save(&mut self, deadline: Deadline) -> Result<BufferedVideo>;
    fn stop(&mut self, keep: bool, deadline: Deadline) -> Result<Option<BufferedVideo>>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingMark {
    pub name: String,
    pub outcome: crate::RecordingOutcome,
    pub at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollingSummary {
    pub id: String,
    pub backend: String,
    pub scope: String,
    pub history_ms: u64,
    pub progress: RollingProgress,
    pub pending: Option<RecordingSummary>,
}

pub(crate) struct ActiveBuffer {
    pub summary: RollingSummary,
    pub handle: Box<dyn RollingBuffer>,
    pub pending: Option<RecordingSummary>,
}
