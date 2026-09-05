use std::path::PathBuf;

pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
pub const MIN_MAX_BYTES: u64 = 1024 * 1024;
pub const MAX_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(super) const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_PENDING_EVENTS: usize = 256;
pub(super) const MAX_CALLS: u64 = 10_000;
pub(super) const MAX_EVENTS: u64 = 100_000;
pub(super) const MAX_BLOBS: usize = 25_000;
pub(super) const MAX_EVENT_BYTES: usize = 64 * 1024;
pub(super) const RESERVE_BYTES: u64 = 128 * 1024;

/// Recording is enabled only when an explicit existing absolute directory is supplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceConfig {
    pub directory: PathBuf,
    pub max_bytes: u64,
}

impl TraceConfig {
    pub fn new(directory: PathBuf, max_bytes: Option<u64>) -> anyhow::Result<Self> {
        let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        anyhow::ensure!(directory.is_absolute(), "trace directory must be absolute");
        anyhow::ensure!(
            directory.parent().is_some(),
            "trace directory cannot be a filesystem root"
        );
        anyhow::ensure!(
            (MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(&max_bytes),
            "trace byte limit must be between {MIN_MAX_BYTES} and {MAX_MAX_BYTES}"
        );
        Ok(Self {
            directory,
            max_bytes,
        })
    }
}
