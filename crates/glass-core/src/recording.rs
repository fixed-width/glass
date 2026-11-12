//! Bounded workflow video, independent of input dispatch and ordinary tracing.

use crate::{Deadline, GlassError, Result};
use serde::{Deserialize, Serialize};

pub const RECORDING_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const RECORDING_DEFAULT_DURATION_MS: u64 = 30_000;
pub const RECORDING_MAX_DURATION_MS: u64 = 60_000;

#[derive(Clone, Debug)]
pub struct RecordingOptions {
    pub id: String,
    pub max_duration_ms: u64,
    pub max_bytes: u64,
}

impl RecordingOptions {
    pub fn validate(&self) -> Result<()> {
        if self.id.len() != 32
            || !self.id.bytes().all(|b| b.is_ascii_hexdigit())
            || !(1000..=RECORDING_MAX_DURATION_MS).contains(&self.max_duration_ms)
            || self.max_bytes == 0
            || self.max_bytes > RECORDING_MAX_BYTES
        {
            return Err(GlassError::Backend("invalid recording options".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingFormat {
    Mp4,
    QuickTime,
}

impl RecordingFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::QuickTime => "mov",
        }
    }
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::QuickTime => "video/quicktime",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Recording,
    Finalized,
    Limited,
    Failed,
    Discarded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingProgress {
    pub state: RecordingState,
    pub elapsed_ms: u64,
    pub bytes: u64,
    pub reason: Option<String>,
}

pub struct RecordedVideo {
    pub format: RecordingFormat,
    pub progress: RecordingProgress,
    pub data: Vec<u8>,
}

/// A backend owns capture and finalization; dropping it must cancel its owned recorder.
pub trait Recording: Send {
    fn progress(&self) -> RecordingProgress;
    fn finish(&mut self, keep: bool, deadline: Deadline) -> Result<RecordedVideo>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingOutcome {
    Success,
    Failure,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingArtifact {
    pub uri: String,
    pub path: String,
    pub path_scope: String,
    pub mime_type: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingSummary {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub scope: String,
    pub progress: RecordingProgress,
    pub outcome: Option<RecordingOutcome>,
    pub kept: bool,
    pub artifact: Option<RecordingArtifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub enabled: bool,
    pub supported: bool,
    pub active: Option<RecordingSummary>,
    pub last: Option<RecordingSummary>,
}

pub trait RecordingSink: Send {
    fn next_id(&self) -> Result<String>;
    fn save(
        &mut self,
        summary: &RecordingSummary,
        video: &RecordedVideo,
    ) -> Result<RecordingArtifact>;
}

pub(crate) struct ActiveRecording {
    pub summary: RecordingSummary,
    pub handle: Box<dyn Recording>,
}

#[derive(Default)]
pub(crate) struct Recordings {
    pub sink: Option<Box<dyn RecordingSink>>,
    pub active: Option<ActiveRecording>,
    pub last: Option<RecordingSummary>,
}

/// Validate the finalized ISO BMFF envelope without decoding untrusted video.
pub fn validate_video(bytes: &[u8]) -> Result<()> {
    let error =
        || GlassError::Backend("recording is empty, truncated, or not finalized video".into());
    let mut offset = 0_usize;
    let (mut ftyp, mut moov, mut media) = (false, false, false);
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset.saturating_add(8))
            .ok_or_else(error)?;
        let size = u32::from_be_bytes(header[..4].try_into().map_err(|_| error())?);
        let (size, header_len) = if size == 1 {
            let wide = bytes
                .get(offset + 8..offset.saturating_add(16))
                .ok_or_else(error)?;
            (
                usize::try_from(u64::from_be_bytes(wide.try_into().map_err(|_| error())?))
                    .map_err(|_| error())?,
                16,
            )
        } else if size == 0 {
            (bytes.len() - offset, 8)
        } else {
            (size as usize, 8)
        };
        let end = offset.checked_add(size).ok_or_else(error)?;
        if size < header_len || end > bytes.len() {
            return Err(error());
        }
        match &header[4..8] {
            b"ftyp" => ftyp = size > header_len,
            b"moov" => moov = size > header_len,
            b"mdat" => media |= size > header_len,
            _ => {}
        }
        offset = end;
    }
    if ftyp && moov && media {
        Ok(())
    } else {
        Err(error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn atom(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut value = ((data.len() + 8) as u32).to_be_bytes().to_vec();
        value.extend(kind);
        value.extend(data);
        value
    }
    #[test]
    fn accepts_finalized_media_and_rejects_a_capture_prefix() {
        let mut bytes = atom(b"ftyp", b"isom");
        bytes.extend(atom(b"mdat", b"frame"));
        assert!(validate_video(&bytes).is_err());
        bytes.extend(atom(b"moov", b"index"));
        assert!(validate_video(&bytes).is_ok());
        bytes.pop();
        assert!(validate_video(&bytes).is_err());
    }
    #[test]
    fn rejects_empty_zero_payload_and_overflowing_boxes() {
        assert!(validate_video(&[]).is_err());
        assert!(validate_video(&[0xff; 16]).is_err());
        let mut bytes = atom(b"ftyp", b"isom");
        bytes.extend(atom(b"mdat", b""));
        bytes.extend(atom(b"moov", b"index"));
        assert!(validate_video(&bytes).is_err());
    }
}
