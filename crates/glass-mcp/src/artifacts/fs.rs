#![expect(
    dead_code,
    reason = "Artifact lifecycle interfaces are consumed by response externalization."
)]

use crate::output::{ArtifactDescriptor, ArtifactKind};
use fs4::FileExt;
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactError {
    CacheRootUnavailable,
    RelativeOverride,
    RootCreateFailed,
    RootCanonicalizeFailed,
    PathRepresentationFailed,
    PermissionFailed,
    LockFailed,
    LeaseUnavailable,
    WriteFailed,
    RenameFailed,
    RollbackFailed,
    StatePoisoned,
    CleanupFailed(std::io::ErrorKind),
    ProtectionRegistrationFailed,
    InvalidOutputState,
    MetadataDidNotStabilize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactReadError {
    ResourceNotFound,
    ExpiredOrUnavailable,
    ReadFailed,
    IntegrityFailed,
}

#[derive(Clone)]
pub(crate) struct ArtifactStore {
    inner: Arc<ArtifactStoreInner>,
}

struct ArtifactStoreInner {
    state: Mutex<StoreState>,
    fault: Option<FaultStage>,
}

struct StoreState {
    root: PathBuf,
    process_dir: PathBuf,
    lease_path: PathBuf,
    lease: Option<File>,
    process_dir_handle: Option<File>,
    server_id: String,
    entries: HashMap<String, RegistryEntry>,
    next_seq: u64,
    limit_bytes: u64,
    availability_error: Option<ArtifactError>,
    closing: bool,
    closed: bool,
}

struct RegistryEntry {
    descriptor: ArtifactDescriptor,
    path: PathBuf,
    size: u64,
    creation_seq: u64,
    pin_count: usize,
}

pub(crate) struct PinGuard {
    store: Weak<ArtifactStoreInner>,
    artifact_ids: Vec<String>,
}

pub(crate) type ResponsePin = PinGuard;
pub(crate) type ReadPin = PinGuard;

#[derive(Debug)]
pub(crate) struct ReadArtifact {
    pub text: String,
    pub mime_type: String,
    pub sha256: String,
    pub untrusted: bool,
    _pin: ReadPin,
}

pub(crate) struct ArtifactDraft {
    pub text: String,
    pub mime_type: String,
    pub untrusted: bool,
    pub kind: ArtifactKind,
    pub content_block: Option<usize>,
    pub original_content_blocks: Vec<usize>,
}

impl ArtifactDraft {
    pub(crate) fn content_block(
        text: impl Into<String>,
        mime_type: impl Into<String>,
        untrusted: bool,
        content_block: usize,
    ) -> Self {
        Self {
            text: text.into(),
            mime_type: mime_type.into(),
            untrusted,
            kind: ArtifactKind::ContentBlock,
            content_block: Some(content_block),
            original_content_blocks: Vec::new(),
        }
    }

    pub(crate) fn response_manifest(
        text: impl Into<String>,
        original_content_blocks: Vec<usize>,
    ) -> Self {
        Self {
            text: text.into(),
            mime_type: "application/vnd.glass.output-manifest+json; charset=utf-8".into(),
            untrusted: true,
            kind: ArtifactKind::ResponseManifest,
            content_block: None,
            original_content_blocks,
        }
    }
}

pub(crate) struct PreparedArtifact {
    id: String,
    final_path: PathBuf,
    temp_path: PathBuf,
    draft: ArtifactDraft,
    descriptor: ArtifactDescriptor,
}

pub(crate) struct PublishedBatch {
    descriptors: Vec<ArtifactDescriptor>,
    pin: ResponsePin,
}

impl PreparedArtifact {
    pub(crate) fn descriptor(&self) -> &ArtifactDescriptor {
        &self.descriptor
    }
}

impl PublishedBatch {
    pub(crate) fn descriptors(&self) -> &[ArtifactDescriptor] {
        &self.descriptors
    }

    pub(crate) fn into_pin(self) -> ResponsePin {
        self.pin
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FaultStage {
    TempCreated(usize),
    TempWritten(usize),
    FinalRenamed(usize),
    GrowDuringRead,
    ReadBodyFails,
    ProcessProtectionThenDirectoryCleanupFails,
    DirectoryCreateThenLeaseCleanupFails,
}

impl FaultStage {
    #[cfg(test)]
    pub(crate) fn publication_stages(count: usize) -> Vec<Self> {
        (0..count)
            .flat_map(|index| {
                [
                    Self::TempCreated(index),
                    Self::TempWritten(index),
                    Self::FinalRenamed(index),
                ]
            })
            .collect()
    }
}

pub(crate) fn default_root_from(cache_dir: &Path) -> PathBuf {
    cache_dir.join("glass").join("artifacts")
}

fn default_root() -> Result<PathBuf, ArtifactError> {
    directories::BaseDirs::new()
        .map(|dirs| default_root_from(dirs.cache_dir()))
        .ok_or(ArtifactError::CacheRootUnavailable)
}

fn configured_root() -> Result<PathBuf, ArtifactError> {
    match std::env::var_os("GLASS_ARTIFACT_DIR") {
        Some(value) if Path::new(&value).is_absolute() => Ok(PathBuf::from(value)),
        Some(_) => Err(ArtifactError::RelativeOverride),
        None => default_root(),
    }
}

impl ArtifactStore {
    pub(crate) fn new(limit_bytes: u64) -> Result<Self, ArtifactError> {
        Self::open(&configured_root()?, limit_bytes, random_id(), None)
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &Path, limit_bytes: u64) -> Result<Self, ArtifactError> {
        Self::open(root, limit_bytes, random_id(), None)
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_id(
        root: &Path,
        limit_bytes: u64,
        server_id: String,
    ) -> Result<Self, ArtifactError> {
        Self::open(root, limit_bytes, server_id, None)
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_fault(
        root: &Path,
        limit_bytes: u64,
        fault: FaultStage,
    ) -> Result<Self, ArtifactError> {
        Self::open(root, limit_bytes, random_id(), Some(fault))
    }

    fn open(
        root: &Path,
        limit_bytes: u64,
        server_id: String,
        fault: Option<FaultStage>,
    ) -> Result<Self, ArtifactError> {
        fs::create_dir_all(root).map_err(|_| ArtifactError::RootCreateFailed)?;
        set_dir_private(root)?;
        let root = fs::canonicalize(root).map_err(|_| ArtifactError::RootCanonicalizeFailed)?;
        require_absolute_utf8(&root)?;
        let process_dir = root.join(format!("server-{server_id}"));
        let lease_path = root.join(format!("server-{server_id}.lease"));
        require_immediate_child(&root, &process_dir)?;
        require_immediate_child(&root, &lease_path)?;

        let lease = open_lease(&lease_path)?;
        let create_result = if fault == Some(FaultStage::DirectoryCreateThenLeaseCleanupFails) {
            Err(ArtifactError::RootCreateFailed)
        } else {
            create_private_dir(&process_dir)
        };
        if let Err(error) = create_result {
            return rollback_initialization(None, &lease_path, lease, fault)
                .map_or_else(Err, |()| Err(error));
        }
        let protection = if fault == Some(FaultStage::ProcessProtectionThenDirectoryCleanupFails) {
            Err(ArtifactError::ProtectionRegistrationFailed)
        } else {
            protect_path(&process_dir)
        };
        if let Err(error) = protection {
            return rollback_initialization(Some(&process_dir), &lease_path, lease, fault)
                .map_or_else(Err, |()| Err(error));
        }
        let process_dir_handle = match open_process_directory(&process_dir) {
            Ok(handle) => handle,
            Err(error) => {
                return rollback_initialization(Some(&process_dir), &lease_path, lease, fault)
                    .map_or_else(Err, |()| Err(error));
            }
        };

        Ok(Self {
            inner: Arc::new(ArtifactStoreInner {
                state: Mutex::new(StoreState {
                    root,
                    process_dir,
                    lease_path,
                    lease: Some(lease),
                    process_dir_handle: Some(process_dir_handle),
                    server_id,
                    entries: HashMap::new(),
                    next_seq: 0,
                    limit_bytes,
                    availability_error: None,
                    closing: false,
                    closed: false,
                }),
                fault,
            }),
        })
    }

    pub(crate) fn prepare(&self, draft: ArtifactDraft) -> Result<PreparedArtifact, ArtifactError> {
        let (server_id, process_dir, available) = {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| ArtifactError::StatePoisoned)?;
            (
                state.server_id.clone(),
                state.process_dir.clone(),
                !state.closing && !state.closed && state.availability_error.is_none(),
            )
        };
        if !available {
            return Err(ArtifactError::InvalidOutputState);
        }
        let id = random_id();
        let final_path = process_dir.join(format!("artifact-{id}.txt"));
        let temp_path = process_dir.join(format!(".artifact-{id}.tmp"));
        require_immediate_child(&process_dir, &final_path)?;
        require_immediate_child(&process_dir, &temp_path)?;
        let sha256 = hash(draft.text.as_bytes());
        let uri = format!("glass-artifact://{server_id}/{id}");
        let descriptor = ArtifactDescriptor::new(
            draft.kind,
            draft.content_block,
            &uri,
            &final_path,
            &draft.mime_type,
            draft.text.len() as u64,
            &sha256,
            draft.untrusted,
            &draft.original_content_blocks,
        )
        .map_err(|_| ArtifactError::PathRepresentationFailed)?;
        Ok(PreparedArtifact {
            id,
            final_path,
            temp_path,
            draft,
            descriptor,
        })
    }

    pub(crate) fn publish(
        &self,
        prepared: Vec<PreparedArtifact>,
    ) -> Result<PublishedBatch, ArtifactError> {
        if prepared.is_empty() {
            return Err(ArtifactError::InvalidOutputState);
        }
        {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| ArtifactError::StatePoisoned)?;
            if state.closing || state.closed || state.availability_error.is_some() {
                return Err(ArtifactError::InvalidOutputState);
            }
            let batch_bytes = prepared.iter().try_fold(0_u64, |sum, item| {
                sum.checked_add(item.draft.text.len() as u64)
                    .ok_or(ArtifactError::InvalidOutputState)
            })?;
            if batch_bytes > state.limit_bytes {
                return Err(ArtifactError::InvalidOutputState);
            }
        }

        let mut created = Vec::with_capacity(prepared.len() * 2);
        let result = self.write_and_rename(&prepared, &mut created);
        if let Err(error) = result {
            return match rollback_paths(&created) {
                Ok(()) => Err(error),
                Err(()) => Err(ArtifactError::RollbackFailed),
            };
        }

        let descriptors = prepared
            .iter()
            .map(|item| item.descriptor.clone())
            .collect::<Vec<_>>();
        let ids = prepared
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        let mut state = match self.inner.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return match rollback_paths(&created) {
                    Ok(()) => Err(ArtifactError::StatePoisoned),
                    Err(()) => Err(ArtifactError::RollbackFailed),
                };
            }
        };
        if state.closing || state.closed || state.availability_error.is_some() {
            drop(state);
            return match rollback_paths(&created) {
                Ok(()) => Err(ArtifactError::InvalidOutputState),
                Err(()) => Err(ArtifactError::RollbackFailed),
            };
        }
        for item in prepared {
            let seq = state.next_seq;
            state.next_seq = state.next_seq.saturating_add(1);
            state.entries.insert(
                item.id,
                RegistryEntry {
                    descriptor: item.descriptor,
                    path: item.final_path,
                    size: item.draft.text.len() as u64,
                    creation_seq: seq,
                    pin_count: 1,
                },
            );
        }
        drop(state);
        Ok(PublishedBatch {
            descriptors,
            pin: PinGuard {
                store: Arc::downgrade(&self.inner),
                artifact_ids: ids,
            },
        })
    }

    fn write_and_rename(
        &self,
        prepared: &[PreparedArtifact],
        created: &mut Vec<PathBuf>,
    ) -> Result<(), ArtifactError> {
        for (index, item) in prepared.iter().enumerate() {
            let mut file = create_private_file(&item.temp_path)?;
            created.push(item.temp_path.clone());
            self.fail_at(FaultStage::TempCreated(index), ArtifactError::WriteFailed)?;
            file.write_all(item.draft.text.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|_| ArtifactError::WriteFailed)?;
            drop(file);
            self.fail_at(FaultStage::TempWritten(index), ArtifactError::WriteFailed)?;
        }
        for (index, item) in prepared.iter().enumerate() {
            fs::rename(&item.temp_path, &item.final_path)
                .map_err(|_| ArtifactError::RenameFailed)?;
            if let Some(path) = created.iter_mut().find(|path| **path == item.temp_path) {
                *path = item.final_path.clone();
            }
            self.fail_at(FaultStage::FinalRenamed(index), ArtifactError::RenameFailed)?;
        }
        Ok(())
    }

    fn fail_at(&self, stage: FaultStage, error: ArtifactError) -> Result<(), ArtifactError> {
        if self.inner.fault == Some(stage) {
            Err(error)
        } else {
            Ok(())
        }
    }

    pub(crate) fn read(&self, uri: &str) -> Result<ReadArtifact, ArtifactReadError> {
        let (id, entry_path, size, sha256, mime_type, untrusted, process_dir, process_handle) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| ArtifactReadError::ExpiredOrUnavailable)?;
            let id = parse_uri(uri, &state.server_id)?;
            let process_dir = state.process_dir.clone();
            let process_handle = state
                .process_dir_handle
                .as_ref()
                .ok_or(ArtifactReadError::ExpiredOrUnavailable)?
                .try_clone()
                .map_err(|_| ArtifactReadError::ReadFailed)?;
            let entry = state
                .entries
                .get_mut(id)
                .ok_or(ArtifactReadError::ResourceNotFound)?;
            entry.pin_count = entry.pin_count.saturating_add(1);
            (
                id.to_owned(),
                entry.path.clone(),
                entry.size,
                entry.descriptor.sha256().to_owned(),
                entry.descriptor.mime_type().to_owned(),
                entry.descriptor.untrusted(),
                process_dir,
                process_handle,
            )
        };
        let pin = PinGuard {
            store: Arc::downgrade(&self.inner),
            artifact_ids: vec![id],
        };
        if entry_path.parent() != Some(process_dir.as_path()) {
            return Err(ArtifactReadError::IntegrityFailed);
        }
        let filename = entry_path
            .file_name()
            .ok_or(ArtifactReadError::IntegrityFailed)?;
        let mut file = open_read_no_follow(&process_handle, &process_dir, filename)?;
        let metadata = file.metadata().map_err(|_| ArtifactReadError::ReadFailed)?;
        if !metadata.file_type().is_file() || metadata.len() != size {
            return Err(ArtifactReadError::IntegrityFailed);
        }
        if self.inner.fault == Some(FaultStage::ReadBodyFails) {
            return Err(ArtifactReadError::ReadFailed);
        }
        if self.inner.fault == Some(FaultStage::GrowDuringRead) {
            let mut writer = OpenOptions::new()
                .append(true)
                .open(&entry_path)
                .map_err(|_| ArtifactReadError::ReadFailed)?;
            writer
                .write_all(b"x")
                .map_err(|_| ArtifactReadError::ReadFailed)?;
        }
        let capacity = usize::try_from(size).map_err(|_| ArtifactReadError::IntegrityFailed)?;
        let read_limit = size
            .checked_add(1)
            .ok_or(ArtifactReadError::IntegrityFailed)?;
        let mut bytes = Vec::with_capacity(capacity);
        Read::by_ref(&mut file)
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|_| ArtifactReadError::ReadFailed)?;
        if bytes.len() as u64 != size || hash(&bytes) != sha256 {
            return Err(ArtifactReadError::IntegrityFailed);
        }
        let text = String::from_utf8(bytes).map_err(|_| ArtifactReadError::IntegrityFailed)?;
        Ok(ReadArtifact {
            text,
            mime_type,
            sha256,
            untrusted,
            _pin: pin,
        })
    }

    pub(crate) fn process_dir(&self) -> PathBuf {
        self.inner.state.lock().map_or_else(
            |poisoned| poisoned.into_inner().process_dir.clone(),
            |state| state.process_dir.clone(),
        )
    }

    pub(crate) fn lease_path(&self) -> PathBuf {
        self.inner.state.lock().map_or_else(
            |poisoned| poisoned.into_inner().lease_path.clone(),
            |state| state.lease_path.clone(),
        )
    }

    pub(crate) fn availability_error(&self) -> Option<ArtifactError> {
        self.inner
            .state
            .lock()
            .map_or(Some(ArtifactError::StatePoisoned), |state| {
                state.availability_error
            })
    }

    pub(crate) fn shutdown(&self) -> Result<(), ArtifactError> {
        let (process_dir, lease_path, process_dir_handle, lease) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| ArtifactError::StatePoisoned)?;
            state.closing = true;
            (
                state.process_dir.clone(),
                state.lease_path.clone(),
                state.process_dir_handle.take(),
                state.lease.take(),
            )
        };
        drop(process_dir_handle);
        remove_owned_dir(&process_dir)?;
        drop(lease);
        fs::remove_file(&lease_path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ArtifactError::StatePoisoned)?;
        state.entries.clear();
        state.closed = true;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn server_id(&self) -> String {
        self.inner.state.lock().map_or_else(
            |poisoned| poisoned.into_inner().server_id.clone(),
            |state| state.server_id.clone(),
        )
    }

    #[cfg(test)]
    pub(crate) fn registry_len(&self) -> usize {
        self.inner.state.lock().map_or_else(
            |poisoned| poisoned.into_inner().entries.len(),
            |state| state.entries.len(),
        )
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        let Some(store) = self.store.upgrade() else {
            return;
        };
        let Ok(mut state) = store.state.lock() else {
            return;
        };
        for id in &self.artifact_ids {
            if let Some(entry) = state.entries.get_mut(id) {
                entry.pin_count = entry.pin_count.saturating_sub(1);
            }
        }
    }
}

impl std::fmt::Debug for ArtifactStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArtifactStore")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PinGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PinGuard").finish_non_exhaustive()
    }
}

fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let mut id = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(id, "{byte:02x}");
    }
    id
}

fn hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn require_absolute_utf8(path: &Path) -> Result<(), ArtifactError> {
    if !path.is_absolute() || path.to_str().is_none() {
        Err(ArtifactError::PathRepresentationFailed)
    } else {
        Ok(())
    }
}

fn require_immediate_child(parent: &Path, child: &Path) -> Result<(), ArtifactError> {
    require_absolute_utf8(child)?;
    if child.parent() == Some(parent) {
        Ok(())
    } else {
        Err(ArtifactError::PathRepresentationFailed)
    }
}

fn open_lease(path: &Path) -> Result<File, ArtifactError> {
    let file = open_private_file(path, false).map_err(|_| ArtifactError::RootCreateFailed)?;
    match FileExt::try_lock(&file) {
        Ok(()) => {
            if let Err(error) = protect_path(path) {
                drop(file);
                return match fs::remove_file(path) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(ArtifactError::CleanupFailed(cleanup.kind())),
                };
            }
            Ok(file)
        }
        Err(fs4::TryLockError::WouldBlock) => Err(ArtifactError::LeaseUnavailable),
        Err(fs4::TryLockError::Error(_)) => Err(ArtifactError::LockFailed),
    }
}

fn create_private_file(path: &Path) -> Result<File, ArtifactError> {
    let file = open_private_file(path, true).map_err(|_| ArtifactError::WriteFailed)?;
    if let Err(error) = protect_path(path) {
        drop(file);
        return match fs::remove_file(path) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(ArtifactError::CleanupFailed(cleanup.kind())),
        };
    }
    Ok(file)
}

fn open_private_file(path: &Path, create_new: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .create_new(create_new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn create_private_dir(path: &Path) -> Result<(), ArtifactError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(path)
            .map_err(|_| ArtifactError::RootCreateFailed)?;
    }
    #[cfg(windows)]
    fs::create_dir(path).map_err(|_| ArtifactError::RootCreateFailed)?;
    Ok(())
}

fn set_dir_private(path: &Path) -> Result<(), ArtifactError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| ArtifactError::PermissionFailed)?;
    }
    #[cfg(windows)]
    protect_path(path)?;
    Ok(())
}

fn protect_path(path: &Path) -> Result<(), ArtifactError> {
    #[cfg(windows)]
    glass_windows::restrict_path_to_current_user(path)
        .map_err(|_| ArtifactError::ProtectionRegistrationFailed)?;
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

fn rollback_initialization(
    process_dir: Option<&Path>,
    lease_path: &Path,
    lease: File,
    fault: Option<FaultStage>,
) -> Result<(), ArtifactError> {
    drop(lease);
    let mut failure = None;
    if let Some(path) = process_dir {
        let result = if fault == Some(FaultStage::ProcessProtectionThenDirectoryCleanupFails) {
            Err(ArtifactError::CleanupFailed(
                std::io::ErrorKind::PermissionDenied,
            ))
        } else {
            remove_owned_dir(path)
        };
        if let Err(error) = result {
            failure = Some(error);
        }
    }
    let lease_result = if fault == Some(FaultStage::DirectoryCreateThenLeaseCleanupFails) {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    } else {
        fs::remove_file(lease_path)
    };
    if let Err(error) = lease_result
        && failure.is_none()
    {
        failure = Some(ArtifactError::CleanupFailed(error.kind()));
    }
    failure.map_or(Ok(()), Err)
}

fn rollback_paths(paths: &[PathBuf]) -> Result<(), ()> {
    let mut failed = false;
    for path in paths.iter().rev() {
        if fs::remove_file(path).is_err_and(|error| error.kind() != std::io::ErrorKind::NotFound) {
            failed = true;
        }
    }
    if failed { Err(()) } else { Ok(()) }
}

fn remove_owned_dir(path: &Path) -> Result<(), ArtifactError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ArtifactError::CleanupFailed(
            std::io::ErrorKind::InvalidInput,
        ));
    }
    for child in fs::read_dir(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))? {
        let child = child.map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        let metadata = child
            .file_type()
            .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        if metadata.is_dir() && !metadata.is_symlink() {
            return Err(ArtifactError::CleanupFailed(
                std::io::ErrorKind::InvalidInput,
            ));
        }
        fs::remove_file(child.path())
            .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
    }
    fs::remove_dir(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))
}

fn parse_uri<'a>(uri: &'a str, server_id: &str) -> Result<&'a str, ArtifactReadError> {
    let prefix = format!("glass-artifact://{server_id}/");
    let id = uri
        .strip_prefix(&prefix)
        .ok_or(ArtifactReadError::ResourceNotFound)?;
    if id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(id)
    } else {
        Err(ArtifactReadError::ResourceNotFound)
    }
}

#[cfg(unix)]
fn open_process_directory(path: &Path) -> Result<File, ArtifactError> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| ArtifactError::RootCanonicalizeFailed)?;
    Ok(File::from(fd))
}

#[cfg(unix)]
fn open_read_no_follow(
    process_handle: &File,
    process_dir: &Path,
    filename: &std::ffi::OsStr,
) -> Result<File, ArtifactReadError> {
    if !directory_handle_matches_path(process_handle, process_dir)? {
        return Err(ArtifactReadError::IntegrityFailed);
    }
    let fd = rustix::fs::openat(
        process_handle,
        Path::new(filename),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| ArtifactReadError::ReadFailed)?;
    let file = File::from(fd);
    if !directory_handle_matches_path(process_handle, process_dir)? {
        return Err(ArtifactReadError::IntegrityFailed);
    }
    Ok(file)
}

#[cfg(unix)]
fn directory_handle_matches_path(handle: &File, path: &Path) -> Result<bool, ArtifactReadError> {
    use std::os::unix::fs::MetadataExt;

    let retained = handle
        .metadata()
        .map_err(|_| ArtifactReadError::ReadFailed)?;
    let current = fs::symlink_metadata(path).map_err(|_| ArtifactReadError::IntegrityFailed)?;
    Ok(current.is_dir()
        && !current.file_type().is_symlink()
        && retained.dev() == current.dev()
        && retained.ino() == current.ino())
}

#[cfg(windows)]
fn open_process_directory(path: &Path) -> Result<File, ArtifactError> {
    glass_windows::open_directory_no_reparse(path)
        .map_err(|_| ArtifactError::RootCanonicalizeFailed)
}

#[cfg(windows)]
fn open_read_no_follow(
    process_handle: &File,
    process_dir: &Path,
    filename: &std::ffi::OsStr,
) -> Result<File, ArtifactReadError> {
    glass_windows::open_file_beneath(process_handle, process_dir, filename).map_err(|error| {
        if error == glass_windows::HostFsError::Integrity {
            ArtifactReadError::IntegrityFailed
        } else {
            ArtifactReadError::ReadFailed
        }
    })
}
