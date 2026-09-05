use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::config::*;

pub(super) const SCHEMA: &str = "glass.trace.v1";

pub(super) fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(super) fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Payload {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Evidence {
    pub label: String,
    pub mime_type: String,
    pub trust: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Payload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Event {
    pub seq: u64,
    pub elapsed_us: u64,
    pub kind: String,
    pub call: Option<u64>,
    pub client: u64,
    pub data: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Limits {
    pub bytes: u64,
    pub payload_bytes: usize,
    pub pending_bytes: usize,
    pub pending_events: usize,
    pub calls: u64,
    pub events: u64,
    pub event_bytes: usize,
    pub blobs: usize,
    pub blocks_per_event: usize,
}

impl Limits {
    pub fn new(bytes: u64) -> Self {
        Self {
            bytes,
            payload_bytes: MAX_PAYLOAD_BYTES,
            pending_bytes: MAX_PENDING_BYTES,
            pending_events: MAX_PENDING_EVENTS,
            calls: MAX_CALLS,
            events: MAX_EVENTS,
            event_bytes: MAX_EVENT_BYTES,
            blobs: MAX_BLOBS,
            blocks_per_event: 128,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub schema: String,
    pub id: String,
    pub started_at: String,
    pub server_version: String,
    pub source_revision: Option<String>,
    pub os: String,
    pub architecture: String,
    pub transport: String,
    pub profile: String,
    pub exclusions: Vec<String>,
    pub limits: Limits,
    pub state: String,
    pub complete: bool,
    pub events: u64,
    pub calls: u64,
    pub omissions: u64,
    pub errors: u64,
    pub stored_bytes: u64,
    pub journal_bytes: u64,
    pub journal_sha256: Option<String>,
    pub finalization: Option<String>,
}
