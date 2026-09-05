use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;

use anyhow::ensure;
use cap_std::fs::Dir;
use sha2::{Digest, Sha256};

use super::config::{MAX_BLOBS, MAX_EVENT_BYTES, RESERVE_BYTES};
use super::format::{Event, Manifest, Payload, digest};
use super::fs;

pub(super) struct Store {
    pub directory: Dir,
    pub blobs: Dir,
    pub manifest: Manifest,
    journal: File,
    journal_hash: Sha256,
    manifest_bytes: u64,
    payloads: BTreeMap<String, Payload>,
    _lease: File,
}

impl Store {
    pub fn new(directory: Dir, manifest: Manifest) -> anyhow::Result<Self> {
        use fs4::FileExt;
        let lease = fs::create_file(&directory, "writer.lease")?;
        FileExt::try_lock(&lease)?;
        let blobs = fs::create_directory(&directory, "blobs")?;
        let journal = fs::create_file(&directory, "events.jsonl")?;
        let mut store = Self {
            directory,
            blobs,
            manifest,
            journal,
            journal_hash: Sha256::new(),
            manifest_bytes: 0,
            payloads: BTreeMap::new(),
            _lease: lease,
        };
        store.write_manifest()?;
        Ok(store)
    }

    pub fn available(&self, bytes: u64) -> bool {
        self.manifest
            .stored_bytes
            .saturating_add(bytes)
            .saturating_add(RESERVE_BYTES)
            <= self.manifest.limits.bytes
    }

    pub fn payload(&mut self, bytes: &[u8]) -> anyhow::Result<Option<Payload>> {
        let sha256 = digest(bytes);
        if let Some(payload) = self.payloads.get(&sha256) {
            return Ok(Some(payload.clone()));
        }
        if self.payloads.len() >= MAX_BLOBS || !self.available(bytes.len() as u64) {
            return Ok(None);
        }
        let name = format!("{sha256}.bin");
        fs::write_atomic(&self.blobs, &name, bytes)?;
        self.manifest.stored_bytes += bytes.len() as u64;
        let payload = Payload {
            path: format!("blobs/{name}"),
            bytes: bytes.len() as u64,
            sha256: sha256.clone(),
        };
        self.payloads.insert(sha256, payload.clone());
        Ok(Some(payload))
    }

    pub fn event(&mut self, mut event: Event, terminal: bool) -> anyhow::Result<bool> {
        event.seq = self.manifest.events + 1;
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= MAX_EVENT_BYTES,
            "trace event exceeds its size limit"
        );
        if !terminal && !self.available(bytes.len() as u64) {
            return Ok(false);
        }
        ensure!(
            self.manifest.stored_bytes + bytes.len() as u64 <= self.manifest.limits.bytes,
            "trace terminal record exceeds byte limit"
        );
        self.journal.write_all(&bytes)?;
        self.journal.flush()?;
        self.journal_hash.update(&bytes);
        self.manifest.events += 1;
        self.manifest.journal_bytes += bytes.len() as u64;
        self.manifest.stored_bytes += bytes.len() as u64;
        Ok(true)
    }

    pub fn write_manifest(&mut self) -> anyhow::Result<()> {
        // Fixed-width accounting converges after at most a few decimal-length changes.
        let without = self
            .manifest
            .stored_bytes
            .saturating_sub(self.manifest_bytes);
        let mut bytes = serde_json::to_vec(&self.manifest)?;
        for _ in 0..4 {
            self.manifest.stored_bytes = without + bytes.len() as u64;
            bytes = serde_json::to_vec(&self.manifest)?;
        }
        ensure!(
            self.manifest.stored_bytes + bytes.len() as u64 <= self.manifest.limits.bytes,
            "trace manifest exceeds byte limit"
        );
        fs::write_atomic(&self.directory, "manifest.json", &bytes)?;
        self.manifest_bytes = bytes.len() as u64;
        Ok(())
    }

    pub fn finish(&mut self) -> anyhow::Result<()> {
        self.journal.flush()?;
        self.manifest.journal_sha256 =
            Some(super::format::hex(&self.journal_hash.clone().finalize()));
        self.write_manifest()
    }
}
