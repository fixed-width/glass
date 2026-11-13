//! Operator-enabled persistent movie storage; application names never form paths.

use crate::trace::fs;
use anyhow::ensure;
use cap_std::fs::Dir;
use glass_core::{GlassError, RecordedVideo, RecordingArtifact, RecordingSink, RecordingSummary};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CLIPS: usize = 1024;
const MAX_METADATA_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingConfig {
    pub directory: PathBuf,
}

impl RecordingConfig {
    pub fn new(directory: PathBuf) -> anyhow::Result<Self> {
        ensure!(
            directory.is_absolute() && directory.parent().is_some(),
            "recording directory must be an absolute non-root path"
        );
        ensure!(
            directory.to_str().is_some(),
            "recording directory must be UTF-8"
        );
        Ok(Self { directory })
    }
}

#[derive(Default)]
struct State {
    bytes: u64,
    clips: HashMap<String, RecordingArtifact>,
}
struct Inner {
    root: PathBuf,
    anchor: Dir,
    directory: Dir,
    path: PathBuf,
    server_id: String,
    state: Mutex<State>,
}
#[derive(Clone)]
pub(crate) struct RecordingStore(Arc<Inner>);

impl RecordingStore {
    pub(crate) fn open(config: &RecordingConfig) -> anyhow::Result<Self> {
        RecordingConfig::new(config.directory.clone())?;
        let anchor = fs::open_directory(&config.directory)?;
        fs::check_owner(&anchor)?;
        let server_id = crate::artifacts::new_server_id();
        let name = format!("recordings-{server_id}");
        let directory = fs::create_directory(&anchor, &name)?;
        Ok(Self(Arc::new(Inner {
            root: config.directory.clone(),
            anchor,
            directory,
            path: config.directory.join(name),
            server_id,
            state: Mutex::new(State::default()),
        })))
    }
    pub(crate) fn root(&self) -> &std::path::Path {
        &self.0.root
    }
    fn check_anchor(&self) -> anyhow::Result<()> {
        let current = fs::open_directory(&self.0.root)?;
        ensure!(
            fs::same_directory(&self.0.anchor, &current)?,
            "recording root identity changed"
        );
        let child = fs::open_directory(&self.0.path)?;
        ensure!(
            fs::same_directory(&self.0.directory, &child)?,
            "recording directory identity changed"
        );
        Ok(())
    }
    pub(crate) fn read(&self, uri: &str) -> anyhow::Result<(RecordingArtifact, Vec<u8>)> {
        let prefix = format!("glass-recording://{}/", self.0.server_id);
        let id = uri
            .strip_prefix(&prefix)
            .ok_or_else(|| anyhow::anyhow!("recording resource not found"))?;
        ensure!(
            id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "recording resource not found"
        );
        self.check_anchor()?;
        let state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("recording storage unavailable"))?;
        let artifact = state
            .clips
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("recording resource not found"))?
            .clone();
        let name = std::path::Path::new(&artifact.path)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid stored movie name"))?;
        let data = fs::read_bounded(
            &self.0.directory,
            name,
            glass_core::recording::RECORDING_MAX_BYTES,
        )?;
        ensure!(
            data.len() as u64 == artifact.bytes && digest(&data) == artifact.sha256,
            "recording integrity check failed"
        );
        Ok((artifact, data))
    }
}

fn backend(error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!("recording storage: {error}"))
}

impl RecordingSink for RecordingStore {
    fn next_id(&self) -> glass_core::Result<String> {
        self.check_anchor().map_err(backend)?;
        let state = self.0.state.lock().map_err(backend)?;
        if state.bytes + glass_core::recording::RECORDING_MAX_BYTES + MAX_METADATA_BYTES > MAX_BYTES
            || state.clips.len() >= MAX_CLIPS
        {
            return Err(backend(
                "retained recording allowance exhausted; restart with a fresh allowance after reviewing clips",
            ));
        }
        Ok(crate::artifacts::new_server_id())
    }
    fn save(
        &mut self,
        summary: &RecordingSummary,
        video: &RecordedVideo,
    ) -> glass_core::Result<RecordingArtifact> {
        self.check_anchor().map_err(backend)?;
        let mut state = self.0.state.lock().map_err(backend)?;
        if summary.id.len() != 32 || !summary.id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(backend("invalid recording identifier"));
        }
        let bytes = video.data.len() as u64;
        if bytes > glass_core::recording::RECORDING_MAX_BYTES
            || state.bytes + bytes > MAX_BYTES
            || state.clips.len() >= MAX_CLIPS
        {
            return Err(backend("retained recording allowance exhausted"));
        }
        let filename = format!("{}.{}", summary.id, video.format.extension());
        let artifact = RecordingArtifact {
            uri: format!("glass-recording://{}/{}", self.0.server_id, summary.id),
            path: self.0.path.join(&filename).to_string_lossy().into_owned(),
            path_scope: "server".into(),
            mime_type: video.format.mime_type().into(),
            bytes,
            sha256: digest(&video.data),
        };
        let mut metadata = summary.clone();
        metadata.artifact = Some(artifact.clone());
        metadata.kept = true;
        let metadata = serde_json::to_vec_pretty(&metadata).map_err(backend)?;
        if metadata.len() as u64 > MAX_METADATA_BYTES
            || state.bytes + bytes + metadata.len() as u64 > MAX_BYTES
        {
            return Err(backend(
                "recording metadata or retained byte allowance exceeded",
            ));
        }
        let mut created = false;
        let metadata_name = format!("{}.json", summary.id);
        let mut metadata_created = false;
        let written = (|| -> anyhow::Result<()> {
            let mut file = fs::create_file(&self.0.directory, &filename)?;
            created = true;
            file.write_all(&video.data)?;
            file.sync_all()?;
            let mut sidecar = fs::create_file(&self.0.directory, &metadata_name)?;
            metadata_created = true;
            sidecar.write_all(&metadata)?;
            sidecar.sync_all()?;
            Ok(())
        })();
        if let Err(error) = written {
            if created {
                let _ = self.0.directory.remove_file(&filename);
            }
            if metadata_created {
                let _ = self.0.directory.remove_file(&metadata_name);
            }
            return Err(backend(error));
        }
        state.bytes += bytes + metadata.len() as u64;
        state.clips.insert(summary.id.clone(), artifact.clone());
        Ok(artifact)
    }
}

fn digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    Sha256::digest(bytes)
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use glass_core::{RecordingFormat, RecordingOutcome, RecordingProgress, RecordingState};
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn private_root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }
    fn summary(id: String) -> RecordingSummary {
        RecordingSummary {
            id,
            name: "../a workflow, not a filename".into(),
            backend: "android".into(),
            scope: "device_display".into(),
            progress: RecordingProgress {
                state: RecordingState::Finalized,
                elapsed_ms: 1000,
                bytes: 5,
                reason: None,
            },
            outcome: Some(RecordingOutcome::Failure),
            kept: true,
            artifact: None,
            marks: Vec::new(),
            window: None,
        }
    }
    fn video() -> RecordedVideo {
        RecordedVideo {
            format: RecordingFormat::Mp4,
            progress: summary("id".into()).progress,
            data: b"movie".to_vec(),
        }
    }
    #[test]
    fn retained_movies_survive_store_drop_and_are_integrity_checked() {
        let root = private_root();
        let path = root.path().canonicalize().unwrap();
        let mut store = RecordingStore::open(&RecordingConfig::new(path).unwrap()).unwrap();
        let artifact = store
            .save(&summary(store.next_id().unwrap()), &video())
            .unwrap();
        assert_eq!(
            std::fs::metadata(&artifact.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(store.read(&artifact.uri).unwrap().1, b"movie");
        assert!(
            store
                .read("glass-recording://other/123456789012345678901234567890ab")
                .is_err()
        );
        std::fs::write(&artifact.path, b"changed").unwrap();
        assert!(store.read(&artifact.uri).is_err());
        drop(store);
        assert!(std::path::Path::new(&artifact.path).exists());
    }
    #[test]
    fn collision_never_removes_an_existing_movie_and_names_cannot_escape() {
        let root = private_root();
        let mut store = RecordingStore::open(
            &RecordingConfig::new(root.path().canonicalize().unwrap()).unwrap(),
        )
        .unwrap();
        let metadata = summary(store.next_id().unwrap());
        let first = store.save(&metadata, &video()).unwrap();
        assert!(store.save(&metadata, &video()).is_err());
        assert_eq!(store.read(&first.uri).unwrap().1, b"movie");
        assert!(
            store
                .save(&summary("../../outside".into()), &video())
                .is_err()
        );
        assert!(!root.path().join("outside.mp4").exists());
    }
    #[test]
    fn refuses_symlinks_writable_roots_and_rebound_directories() {
        let parent = tempfile::tempdir().unwrap();
        let parent_path = parent.path().canonicalize().unwrap();
        let root = parent_path.join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(RecordingStore::open(&RecordingConfig::new(root.clone()).unwrap()).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = parent_path.join("link");
        symlink(&root, &alias).unwrap();
        assert!(RecordingStore::open(&RecordingConfig::new(alias).unwrap()).is_err());
        let store = RecordingStore::open(&RecordingConfig::new(root.clone()).unwrap()).unwrap();
        std::fs::rename(&root, parent_path.join("moved")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(store.next_id().is_err());
    }
    #[test]
    fn quota_is_reserved_before_native_capture_starts() {
        let root = private_root();
        let store = RecordingStore::open(
            &RecordingConfig::new(root.path().canonicalize().unwrap()).unwrap(),
        )
        .unwrap();
        store.0.state.lock().unwrap().bytes = MAX_BYTES;
        assert!(store.next_id().is_err());
    }

    #[test]
    fn metadata_collision_is_not_overwritten() {
        let root = private_root();
        let mut store = RecordingStore::open(
            &RecordingConfig::new(root.path().canonicalize().unwrap()).unwrap(),
        )
        .unwrap();
        let summary = summary(store.next_id().unwrap());
        let name = format!("{}.json", summary.id);
        let mut original = fs::create_file(&store.0.directory, &name).unwrap();
        original.write_all(b"existing metadata").unwrap();
        drop(original);
        assert!(store.save(&summary, &video()).is_err());
        assert_eq!(
            fs::read_bounded(&store.0.directory, &name, 100).unwrap(),
            b"existing metadata"
        );
        assert!(!store.0.path.join(format!("{}.mp4", summary.id)).exists());
    }
}
