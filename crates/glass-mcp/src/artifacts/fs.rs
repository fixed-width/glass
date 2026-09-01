#![expect(
    dead_code,
    reason = "Artifact lifecycle interfaces are consumed by response externalization."
)]

use crate::output::{ArtifactDescriptor, ArtifactKind};
use fs4::FileExt;
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

const MAX_OWNED_DIRECTORY_ENTRIES: usize = 16 * 1024;

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
    fault_fired: AtomicBool,
    #[cfg(test)]
    test_hook: Mutex<Option<TestHook>>,
}

#[cfg(test)]
type TestHook = (TestHookPoint, Box<dyn FnOnce() + Send>);

struct StoreState {
    root: PathBuf,
    root_handle: Option<File>,
    process_dir: PathBuf,
    lease_path: PathBuf,
    lease: Option<File>,
    process_dir_handle: Option<File>,
    server_id: String,
    entries: HashMap<String, RegistryEntry>,
    next_seq: u64,
    limit_bytes: u64,
    availability_error: Option<ArtifactError>,
    lifecycle: Lifecycle,
    #[cfg(unix)]
    lease_quarantine: Option<OsString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestHookPoint {
    AfterAccountingEnumeration,
    BeforeEvictionRemove,
    AfterCleanupEnumeration,
    BeforeCleanupRemove,
    AfterScavengeEnumeration,
    BeforeProcessDirectoryRemove,
    BeforeLeaseRemove,
    AfterLeaseQuarantine,
    AfterQuarantineCanonicalAbsent,
    AfterRecoveryReservation,
    AfterRecoveryProcessRemoval,
    AfterRecoveryGuardRemoval,
}

#[cfg(test)]
thread_local! {
    static FS_TEST_HOOK: std::cell::RefCell<Option<TestHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_fs_test_hook(point: TestHookPoint, hook: impl FnOnce() + Send + 'static) {
    FS_TEST_HOOK.with(|slot| *slot.borrow_mut() = Some((point, Box::new(hook))));
}

fn fire_fs_test_hook(point: TestHookPoint) {
    #[cfg(test)]
    FS_TEST_HOOK.with(|slot| {
        let scheduled = slot.borrow_mut().take();
        match scheduled {
            Some((scheduled, hook)) if scheduled == point => hook(),
            other => *slot.borrow_mut() = other,
        }
    });
    #[cfg(not(test))]
    let _ = point;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Open,
    Closing(CleanupProgress),
    Closed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CleanupProgress {
    contents_removed: bool,
    directory_removed: bool,
    handles_closed: bool,
    lease_removed: bool,
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
    PublicationRollbackCleanupFails,
    #[cfg(test)]
    RetentionAfterRegistryInsertion,
    #[cfg(test)]
    CommittedBatchRollbackCleanupFails,
    #[cfg(test)]
    PreparePathRepresentationFails,
    ShutdownRemoveContents,
    ShutdownRemoveDirectory,
    ShutdownCloseHandles,
    ShutdownRemoveLease,
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
        Self::open(&configured_root()?, limit_bytes, new_server_id(), None)
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &Path, limit_bytes: u64) -> Result<Self, ArtifactError> {
        Self::open(root, limit_bytes, new_server_id(), None)
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
        Self::open(root, limit_bytes, new_server_id(), Some(fault))
    }

    #[cfg(test)]
    pub(crate) fn scavenge_for_test(root: &Path) -> Result<(), ArtifactError> {
        scavenge_root(root, None)
    }

    #[cfg(test)]
    pub(crate) fn scavenge_with_lease_open_fault_for_test(
        root: &Path,
        lease_path: &Path,
    ) -> Result<(), ArtifactError> {
        scavenge_root(root, Some(lease_path))
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
        let root_handle = open_process_directory(&root)?;
        scavenge_root_from_handle(&root, &root_handle, None)?;
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
                    root_handle: Some(root_handle),
                    process_dir,
                    lease_path,
                    lease: Some(lease),
                    process_dir_handle: Some(process_dir_handle),
                    server_id,
                    entries: HashMap::new(),
                    next_seq: 0,
                    limit_bytes,
                    availability_error: None,
                    lifecycle: Lifecycle::Open,
                    #[cfg(unix)]
                    lease_quarantine: None,
                }),
                fault,
                fault_fired: AtomicBool::new(false),
                #[cfg(test)]
                test_hook: Mutex::new(None),
            }),
        })
    }

    pub(crate) fn prepare(&self, draft: ArtifactDraft) -> Result<PreparedArtifact, ArtifactError> {
        #[cfg(test)]
        if self.inner.fault == Some(FaultStage::PreparePathRepresentationFails)
            && !self.inner.fault_fired.swap(true, Ordering::AcqRel)
        {
            return Err(ArtifactError::PathRepresentationFailed);
        }
        let (server_id, process_dir, available) = {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| ArtifactError::StatePoisoned)?;
            (
                state.server_id.clone(),
                state.process_dir.clone(),
                state.lifecycle == Lifecycle::Open && state.availability_error.is_none(),
            )
        };
        if !available {
            return Err(ArtifactError::InvalidOutputState);
        }
        let id = new_server_id();
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
            if state.lifecycle != Lifecycle::Open || state.availability_error.is_some() {
                return Err(ArtifactError::InvalidOutputState);
            }
        }

        let mut created = Vec::with_capacity(prepared.len() * 2);
        let result = self.write_and_rename(&prepared, &mut created);
        if let Err(error) = result {
            return self.rollback_publication(&created, error);
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
                return self.rollback_publication(&created, ArtifactError::StatePoisoned);
            }
        };
        if state.lifecycle != Lifecycle::Open || state.availability_error.is_some() {
            drop(state);
            return self.rollback_publication(&created, ArtifactError::InvalidOutputState);
        }
        let count = u64::try_from(prepared.len()).map_err(|_| ArtifactError::InvalidOutputState)?;
        let Some(next_seq) = state.next_seq.checked_add(count) else {
            drop(state);
            return self.rollback_publication(&created, ArtifactError::InvalidOutputState);
        };
        let first_seq = state.next_seq;
        state.next_seq = next_seq;
        for (seq, item) in (first_seq..next_seq).zip(prepared) {
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
        let injected_retention_failure = match self.inner.fault {
            #[cfg(test)]
            Some(FaultStage::RetentionAfterRegistryInsertion)
            | Some(FaultStage::CommittedBatchRollbackCleanupFails) => true,
            #[cfg(test)]
            Some(FaultStage::PreparePathRepresentationFails) => false,
            Some(
                FaultStage::TempCreated(_)
                | FaultStage::TempWritten(_)
                | FaultStage::FinalRenamed(_)
                | FaultStage::GrowDuringRead
                | FaultStage::ReadBodyFails
                | FaultStage::ProcessProtectionThenDirectoryCleanupFails
                | FaultStage::DirectoryCreateThenLeaseCleanupFails
                | FaultStage::PublicationRollbackCleanupFails
                | FaultStage::ShutdownRemoveContents
                | FaultStage::ShutdownRemoveDirectory
                | FaultStage::ShutdownCloseHandles
                | FaultStage::ShutdownRemoveLease,
            )
            | None => false,
        };
        let retention = if injected_retention_failure {
            self.inner.fault_fired.store(true, Ordering::Release);
            Err(ArtifactError::CleanupFailed(
                std::io::ErrorKind::PermissionDenied,
            ))
        } else {
            self.enforce_retention()
        };
        if let Err(error) = retention {
            return self.rollback_committed_batch(&ids, &created, error);
        }
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
            let fail_rollback = self.inner.fault
                == Some(FaultStage::PublicationRollbackCleanupFails)
                && self
                    .inner
                    .state
                    .lock()
                    .is_ok_and(|state| !state.entries.is_empty());
            if fail_rollback {
                return Err(ArtifactError::WriteFailed);
            }
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
        if self.inner.fault == Some(stage)
            && self
                .inner
                .fault_fired
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            Err(error)
        } else {
            Ok(())
        }
    }

    fn rollback_publication(
        &self,
        paths: &[PathBuf],
        original: ArtifactError,
    ) -> Result<PublishedBatch, ArtifactError> {
        let injected_failure = self.inner.fault
            == Some(FaultStage::PublicationRollbackCleanupFails)
            && self
                .inner
                .fault_fired
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        if !injected_failure && rollback_paths(paths).is_ok() {
            return Err(original);
        }
        if let Ok(mut state) = self.inner.state.lock() {
            state.availability_error = Some(ArtifactError::RollbackFailed);
        }
        Err(ArtifactError::RollbackFailed)
    }

    fn rollback_committed_batch(
        &self,
        ids: &[String],
        paths: &[PathBuf],
        original: ArtifactError,
    ) -> Result<PublishedBatch, ArtifactError> {
        let registry_rolled_back = match self.inner.state.lock() {
            Ok(mut state) => {
                for id in ids {
                    state.entries.remove(id);
                }
                true
            }
            Err(mut poisoned) => {
                poisoned.get_mut().availability_error = Some(ArtifactError::RollbackFailed);
                false
            }
        };
        #[cfg(test)]
        let injected_cleanup_failure =
            self.inner.fault == Some(FaultStage::CommittedBatchRollbackCleanupFails);
        #[cfg(not(test))]
        let injected_cleanup_failure = false;
        let paths_rolled_back = !injected_cleanup_failure && rollback_paths(paths).is_ok();
        if registry_rolled_back && paths_rolled_back {
            return Err(original);
        }
        self.mark_unavailable(ArtifactError::RollbackFailed);
        Err(ArtifactError::RollbackFailed)
    }

    pub(crate) fn read(&self, uri: &str) -> Result<ReadArtifact, ArtifactReadError> {
        let (id, entry_path, size, sha256, mime_type, untrusted, process_dir, process_handle) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| ArtifactReadError::ExpiredOrUnavailable)?;
            if state.lifecycle != Lifecycle::Open {
                return Err(ArtifactReadError::ExpiredOrUnavailable);
            }
            let id = classify_uri(uri, &state.server_id)?;
            let process_dir = state.process_dir.clone();
            let process_handle = state
                .process_dir_handle
                .as_ref()
                .ok_or(ArtifactReadError::ExpiredOrUnavailable)?
                .try_clone()
                .map_err(|_| ArtifactReadError::ReadFailed)?;
            if !state.entries.contains_key(id) {
                return Err(ArtifactReadError::ExpiredOrUnavailable);
            }
            let Some(entry) = state.entries.get_mut(id) else {
                return Err(ArtifactReadError::ExpiredOrUnavailable);
            };
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

    pub(crate) fn mark_unavailable(&self, error: ArtifactError) {
        match self.inner.state.lock() {
            Ok(mut state) => state.availability_error = Some(error),
            Err(mut poisoned) => poisoned.get_mut().availability_error = Some(error),
        }
    }

    pub(crate) fn enforce_retention(&self) -> Result<(), ArtifactError> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ArtifactError::StatePoisoned)?;
        if state.lifecycle != Lifecycle::Open {
            return Ok(());
        }
        loop {
            let process_handle = state
                .process_dir_handle
                .as_ref()
                .ok_or(ArtifactError::InvalidOutputState)?;
            if process_file_bytes_from_handle(process_handle, &state.process_dir)?
                <= state.limit_bytes
            {
                return Ok(());
            }
            let candidate = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.pin_count == 0)
                .min_by_key(|(_, entry)| entry.creation_seq)
                .map(|(id, entry)| (id.clone(), entry.path.clone()));
            let Some((id, path)) = candidate else {
                return Ok(());
            };
            let filename = path.file_name().ok_or(ArtifactError::InvalidOutputState)?;
            #[cfg(windows)]
            let retained = glass_windows::open_file_child(process_handle, filename)
                .map_err(map_host_cleanup)?;
            fire_fs_test_hook(TestHookPoint::BeforeEvictionRemove);
            #[cfg(unix)]
            remove_regular_file_from_handle(process_handle, &state.process_dir, filename)?;
            #[cfg(windows)]
            glass_windows::remove_by_handle(&retained).map_err(map_host_cleanup)?;
            state.entries.remove(&id);
        }
    }

    #[cfg(test)]
    pub(crate) fn total_file_bytes(&self) -> Result<u64, ArtifactError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| ArtifactError::StatePoisoned)?;
        let handle = state
            .process_dir_handle
            .as_ref()
            .ok_or(ArtifactError::InvalidOutputState)?;
        process_file_bytes_from_handle(handle, &state.process_dir)
    }

    pub(crate) fn shutdown(&self) -> Result<(), ArtifactError> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ArtifactError::StatePoisoned)?;
        let mut progress = match state.lifecycle {
            Lifecycle::Open => CleanupProgress::default(),
            Lifecycle::Closing(progress) => progress,
            Lifecycle::Closed => return Ok(()),
        };
        state.lifecycle = Lifecycle::Closing(progress);

        if !progress.contents_removed {
            self.fail_at(
                FaultStage::ShutdownRemoveContents,
                ArtifactError::CleanupFailed(std::io::ErrorKind::PermissionDenied),
            )?;
            let result = match state.process_dir_handle.as_ref() {
                Some(handle) => remove_owned_contents_from_handle(handle, &state.process_dir),
                None => Err(ArtifactError::CleanupFailed(std::io::ErrorKind::NotFound)),
            };
            match result {
                Ok(()) => {}
                Err(ArtifactError::CleanupFailed(std::io::ErrorKind::NotFound)) => {}
                Err(error) => return Err(error),
            }
            progress.contents_removed = true;
            state.lifecycle = Lifecycle::Closing(progress);
        }
        if !progress.directory_removed {
            self.fail_at(
                FaultStage::ShutdownRemoveDirectory,
                ArtifactError::CleanupFailed(std::io::ErrorKind::PermissionDenied),
            )?;
            let root_handle = state
                .root_handle
                .as_ref()
                .ok_or(ArtifactError::InvalidOutputState)?;
            let process_name = state
                .process_dir
                .file_name()
                .ok_or(ArtifactError::InvalidOutputState)?;
            fire_fs_test_hook(TestHookPoint::BeforeProcessDirectoryRemove);
            match remove_directory_from_handle(root_handle, &state.root, process_name) {
                Ok(()) => {}
                Err(ArtifactError::CleanupFailed(std::io::ErrorKind::NotFound)) => {}
                Err(error) => return Err(error),
            }
            progress.directory_removed = true;
            state.lifecycle = Lifecycle::Closing(progress);
        }
        if !progress.lease_removed {
            self.fail_at(
                FaultStage::ShutdownRemoveLease,
                ArtifactError::CleanupFailed(std::io::ErrorKind::PermissionDenied),
            )?;
            let root_handle = state
                .root_handle
                .as_ref()
                .ok_or(ArtifactError::InvalidOutputState)?
                .try_clone()
                .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
            let lease_name = state
                .lease_path
                .file_name()
                .ok_or(ArtifactError::InvalidOutputState)?
                .to_os_string();
            let lease = state
                .lease
                .as_ref()
                .ok_or(ArtifactError::InvalidOutputState)?
                .try_clone()
                .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
            #[cfg(unix)]
            let server_id = state.server_id.clone();
            fire_fs_test_hook(TestHookPoint::BeforeLeaseRemove);
            #[cfg(unix)]
            let result = remove_locked_lease_for_shutdown(
                &root_handle,
                &server_id,
                &lease_name,
                &lease,
                &mut state.lease_quarantine,
                || self.fire_test_hook(TestHookPoint::AfterLeaseQuarantine),
            );
            #[cfg(windows)]
            let result =
                remove_locked_lease_from_handle(&root_handle, &state.root, &lease_name, &lease);
            match result {
                Ok(()) => {}
                Err(ArtifactError::CleanupFailed(std::io::ErrorKind::NotFound)) => {}
                Err(error) => return Err(error),
            }
            progress.lease_removed = true;
            state.lifecycle = Lifecycle::Closing(progress);
        }
        if !progress.handles_closed {
            self.fail_at(
                FaultStage::ShutdownCloseHandles,
                ArtifactError::CleanupFailed(std::io::ErrorKind::PermissionDenied),
            )?;
            drop(state.process_dir_handle.take());
            drop(state.lease.take());
            progress.handles_closed = true;
            state.lifecycle = Lifecycle::Closing(progress);
        }
        state.entries.clear();
        drop(state.root_handle.take());
        state.lifecycle = Lifecycle::Closed;
        Ok(())
    }

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

    #[cfg(test)]
    pub(crate) fn poison_state_for_test(&self) {
        let inner = Arc::clone(&self.inner);
        let _ = std::panic::catch_unwind(move || {
            let _guard = inner.state.lock().unwrap();
            panic!("poison artifact state for drop coverage");
        });
    }

    #[cfg(test)]
    pub(crate) fn set_next_seq_for_test(&self, next_seq: u64) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.next_seq = next_seq;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_hook(&self, point: TestHookPoint, hook: impl FnOnce() + Send + 'static) {
        if let Ok(mut slot) = self.inner.test_hook.lock() {
            *slot = Some((point, Box::new(hook)));
        }
    }

    fn fire_test_hook(&self, point: TestHookPoint) {
        #[cfg(test)]
        let hook = self
            .inner
            .test_hook
            .lock()
            .ok()
            .and_then(|mut slot| match slot.as_ref() {
                Some((scheduled, _)) if *scheduled == point => slot.take().map(|(_, hook)| hook),
                _ => None,
            });
        #[cfg(test)]
        {
            if let Some(hook) = hook {
                hook();
            }
        }
        #[cfg(not(test))]
        let _ = point;
    }

    #[cfg(all(test, unix))]
    pub(crate) fn lease_quarantine_count_for_test(&self) -> usize {
        self.inner
            .state
            .lock()
            .map_or(0, |state| usize::from(state.lease_quarantine.is_some()))
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        let Some(store) = self.store.upgrade() else {
            return;
        };
        {
            let Ok(mut state) = store.state.lock() else {
                return;
            };
            for id in &self.artifact_ids {
                if let Some(entry) = state.entries.get_mut(id) {
                    entry.pin_count = entry.pin_count.saturating_sub(1);
                }
            }
        }
        let store = ArtifactStore { inner: store };
        let _ = store.enforce_retention();
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

pub(crate) fn new_server_id() -> String {
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
    if !safe_directory_metadata(&metadata) {
        return Err(ArtifactError::CleanupFailed(
            std::io::ErrorKind::InvalidInput,
        ));
    }
    remove_owned_contents(path)?;
    fs::remove_dir(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))
}

fn remove_owned_contents(path: &Path) -> Result<(), ArtifactError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
    if !safe_directory_metadata(&metadata) {
        return Err(ArtifactError::CleanupFailed(
            std::io::ErrorKind::InvalidInput,
        ));
    }
    for child in fs::read_dir(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))? {
        let child = child.map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        remove_owned_entry(&child.path())?;
    }
    Ok(())
}

fn remove_owned_entry(path: &Path) -> Result<(), ArtifactError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return remove_link_no_follow(path, &metadata);
    }
    if metadata.is_dir() {
        remove_owned_contents(path)?;
        return fs::remove_dir(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()));
    }
    if !metadata.is_dir() {
        return fs::remove_file(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()));
    }
    Err(ArtifactError::CleanupFailed(
        std::io::ErrorKind::InvalidInput,
    ))
}

#[cfg(unix)]
fn remove_link_no_follow(path: &Path, _metadata: &fs::Metadata) -> Result<(), ArtifactError> {
    fs::remove_file(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))
}

#[cfg(windows)]
fn remove_link_no_follow(path: &Path, metadata: &fs::Metadata) -> Result<(), ArtifactError> {
    use std::os::windows::fs::MetadataExt;
    let result = if metadata.file_attributes() & 0x10 != 0 {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| ArtifactError::CleanupFailed(error.kind()))
}

fn process_file_bytes_no_follow(path: &Path) -> Result<u64, ArtifactError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
    if !safe_directory_metadata(&metadata) {
        return Err(ArtifactError::CleanupFailed(
            std::io::ErrorKind::InvalidInput,
        ));
    }
    let mut total = 0_u64;
    for child in fs::read_dir(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))? {
        let child = child.map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        let metadata = fs::symlink_metadata(child.path())
            .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            total = total
                .checked_add(process_file_bytes_no_follow(&child.path())?)
                .ok_or(ArtifactError::InvalidOutputState)?;
        } else if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or(ArtifactError::InvalidOutputState)?;
        }
    }
    Ok(total)
}

fn remove_regular_file_no_follow(path: &Path) -> Result<(), ArtifactError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ArtifactError::CleanupFailed(
            std::io::ErrorKind::InvalidInput,
        ));
    }
    fs::remove_file(path).map_err(|error| ArtifactError::CleanupFailed(error.kind()))
}

#[cfg(unix)]
fn handle_directory_entries(handle: &File) -> Result<Vec<OsString>, ArtifactError> {
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "linux")]
    let directory = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    let directory = PathBuf::from(format!("/dev/fd/{}", handle.as_raw_fd()));
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(directory).map_err(|error| ArtifactError::CleanupFailed(error.kind()))?
    {
        if entries.len() == MAX_OWNED_DIRECTORY_ENTRIES {
            return Err(ArtifactError::InvalidOutputState);
        }
        entries.push(
            entry
                .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?
                .file_name(),
        );
    }
    Ok(entries)
}

#[cfg(unix)]
fn open_directory_from_handle(parent: &File, name: &OsStr) -> Result<File, ArtifactError> {
    let fd = rustix::fs::openat(
        parent,
        Path::new(name),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| ArtifactError::CleanupFailed(std::io::Error::from(error).kind()))?;
    Ok(File::from(fd))
}

#[cfg(unix)]
fn remove_owned_contents_from_handle(handle: &File, _path: &Path) -> Result<(), ArtifactError> {
    let entries = handle_directory_entries(handle)?;
    fire_fs_test_hook(TestHookPoint::AfterCleanupEnumeration);
    for name in entries {
        match open_directory_from_handle(handle, &name) {
            Ok(child) => {
                remove_owned_contents_from_handle(&child, Path::new("."))?;
                drop(child);
                fire_fs_test_hook(TestHookPoint::BeforeCleanupRemove);
                remove_directory_from_handle(handle, Path::new("."), &name)?;
            }
            Err(ArtifactError::CleanupFailed(std::io::ErrorKind::NotFound)) => {}
            Err(_) => remove_non_directory_from_handle(handle, Path::new("."), &name)?,
        }
    }
    Ok(())
}

#[cfg(unix)]
fn remove_directory_from_handle(
    parent: &File,
    _path: &Path,
    name: &OsStr,
) -> Result<(), ArtifactError> {
    rustix::fs::unlinkat(parent, Path::new(name), rustix::fs::AtFlags::REMOVEDIR)
        .map_err(|error| ArtifactError::CleanupFailed(std::io::Error::from(error).kind()))
}

#[cfg(unix)]
fn remove_non_directory_from_handle(
    parent: &File,
    _path: &Path,
    name: &OsStr,
) -> Result<(), ArtifactError> {
    rustix::fs::unlinkat(parent, Path::new(name), rustix::fs::AtFlags::empty())
        .map_err(|error| ArtifactError::CleanupFailed(std::io::Error::from(error).kind()))
}

#[cfg(unix)]
fn remove_regular_file_from_handle(
    parent: &File,
    _path: &Path,
    name: &OsStr,
) -> Result<(), ArtifactError> {
    let fd = rustix::fs::openat(
        parent,
        Path::new(name),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| ArtifactError::CleanupFailed(std::io::Error::from(error).kind()))?;
    let file = File::from(fd);
    if !file
        .metadata()
        .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?
        .is_file()
    {
        return Err(ArtifactError::CleanupFailed(
            std::io::ErrorKind::InvalidInput,
        ));
    }
    drop(file);
    remove_non_directory_from_handle(parent, Path::new("."), name)
}

#[cfg(unix)]
fn process_file_bytes_from_handle(handle: &File, _path: &Path) -> Result<u64, ArtifactError> {
    let mut total = 0_u64;
    let entries = handle_directory_entries(handle)?;
    fire_fs_test_hook(TestHookPoint::AfterAccountingEnumeration);
    for name in entries {
        if let Ok(child) = open_directory_from_handle(handle, &name) {
            total = total
                .checked_add(process_file_bytes_from_handle(&child, Path::new("."))?)
                .ok_or(ArtifactError::InvalidOutputState)?;
            continue;
        }
        let fd = match rustix::fs::openat(
            handle,
            Path::new(&name),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(_) => continue,
        };
        let metadata = File::from(fd)
            .metadata()
            .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or(ArtifactError::InvalidOutputState)?;
        }
    }
    Ok(total)
}

#[cfg(windows)]
fn remove_owned_contents_from_handle(handle: &File, _path: &Path) -> Result<(), ArtifactError> {
    let entries = glass_windows::directory_entry_records(handle).map_err(map_host_cleanup)?;
    fire_fs_test_hook(TestHookPoint::AfterCleanupEnumeration);
    for entry in entries {
        let file = glass_windows::open_directory_entry(handle, &entry).map_err(map_host_cleanup)?;
        let metadata = file
            .metadata()
            .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        if metadata.is_dir() && !is_reparse_point(&metadata) {
            remove_owned_contents_from_handle(&file, Path::new("."))?;
            fire_fs_test_hook(TestHookPoint::BeforeCleanupRemove);
            glass_windows::remove_by_handle(&file).map_err(map_host_cleanup)?;
        } else {
            glass_windows::remove_by_handle(&file).map_err(map_host_cleanup)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn remove_directory_from_handle(
    parent: &File,
    _path: &Path,
    name: &std::ffi::OsStr,
) -> Result<(), ArtifactError> {
    let child = glass_windows::open_directory_beneath(parent, name).map_err(map_host_cleanup)?;
    glass_windows::remove_by_handle(&child).map_err(map_host_cleanup)
}

#[cfg(windows)]
fn remove_non_directory_from_handle(
    parent: &File,
    _path: &Path,
    name: &std::ffi::OsStr,
) -> Result<(), ArtifactError> {
    let child = glass_windows::open_entry_child(parent, name).map_err(map_host_cleanup)?;
    glass_windows::remove_by_handle(&child).map_err(map_host_cleanup)
}

#[cfg(windows)]
fn remove_regular_file_from_handle(
    parent: &File,
    _path: &Path,
    name: &std::ffi::OsStr,
) -> Result<(), ArtifactError> {
    let child = glass_windows::open_file_child(parent, name).map_err(map_host_cleanup)?;
    glass_windows::remove_by_handle(&child).map_err(map_host_cleanup)
}

#[cfg(windows)]
fn process_file_bytes_from_handle(handle: &File, _path: &Path) -> Result<u64, ArtifactError> {
    let mut total = 0_u64;
    let entries = glass_windows::directory_entry_records(handle).map_err(map_host_cleanup)?;
    fire_fs_test_hook(TestHookPoint::AfterAccountingEnumeration);
    for entry in entries {
        let file = glass_windows::open_directory_entry(handle, &entry).map_err(map_host_cleanup)?;
        let metadata = file
            .metadata()
            .map_err(|error| ArtifactError::CleanupFailed(error.kind()))?;
        if metadata.is_dir() && !is_reparse_point(&metadata) {
            total = total
                .checked_add(process_file_bytes_from_handle(&file, Path::new("."))?)
                .ok_or(ArtifactError::InvalidOutputState)?;
        } else if metadata.is_file() && !is_reparse_point(&metadata) {
            total = total
                .checked_add(metadata.len())
                .ok_or(ArtifactError::InvalidOutputState)?;
        }
    }
    Ok(total)
}

#[cfg(windows)]
fn map_host_cleanup(_error: glass_windows::HostFsError) -> ArtifactError {
    ArtifactError::CleanupFailed(std::io::ErrorKind::Other)
}

fn scavenge_root(root: &Path, lease_open_fault: Option<&Path>) -> Result<(), ArtifactError> {
    let root_handle = open_process_directory(root)?;
    scavenge_root_from_handle(root, &root_handle, lease_open_fault)
}

#[cfg(unix)]
fn scavenge_root_from_handle(
    root: &Path,
    root_handle: &File,
    lease_open_fault: Option<&Path>,
) -> Result<(), ArtifactError> {
    let entries = handle_directory_entries(root_handle)?;
    fire_fs_test_hook(TestHookPoint::AfterScavengeEnumeration);
    let mut quarantines: HashMap<&str, Vec<&OsStr>> = HashMap::new();
    for name in &entries {
        let Some(text) = name.to_str() else {
            continue;
        };
        if let Some(id) = exact_quarantine_id(text) {
            let candidates = quarantines.entry(id).or_default();
            if candidates.len() < 2 {
                candidates.push(name);
            }
        }
    }
    for (id, candidates) in quarantines {
        if candidates.len() != 1 {
            continue;
        }
        let canonical = OsString::from(format!("server-{id}.lease"));
        let quarantine = candidates[0];
        let Ok(fd) = rustix::fs::openat(
            root_handle,
            Path::new(quarantine),
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) else {
            continue;
        };
        let lease = File::from(fd);
        if !lease.metadata().is_ok_and(|metadata| metadata.is_file())
            || FileExt::try_lock(&lease).is_err()
        {
            continue;
        }
        let guard_exists = match rustix::fs::statat(
            root_handle,
            Path::new(&canonical),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(_) => {
                if !relative_file_matches_handle(root_handle, &canonical, &lease) {
                    continue;
                }
                true
            }
            Err(rustix::io::Errno::NOENT) => {
                fire_fs_test_hook(TestHookPoint::AfterQuarantineCanonicalAbsent);
                if rustix::fs::linkat(
                    root_handle,
                    Path::new(quarantine),
                    root_handle,
                    Path::new(&canonical),
                    rustix::fs::AtFlags::empty(),
                )
                .is_err()
                {
                    continue;
                }
                false
            }
            Err(_) => continue,
        };
        if !guard_exists && !relative_file_matches_handle(root_handle, &canonical, &lease) {
            continue;
        }
        fire_fs_test_hook(TestHookPoint::AfterRecoveryReservation);
        let process_name = OsString::from(format!("server-{id}"));
        if let Ok(process) = open_directory_from_handle(root_handle, &process_name) {
            if remove_owned_contents_from_handle(&process, Path::new(".")).is_err() {
                continue;
            }
            drop(process);
            if remove_directory_from_handle(root_handle, root, &process_name).is_err() {
                continue;
            }
        }
        fire_fs_test_hook(TestHookPoint::AfterRecoveryProcessRemoval);
        if !relative_file_matches_handle(root_handle, &canonical, &lease) {
            continue;
        }
        if remove_non_directory_from_handle(root_handle, root, &canonical).is_err() {
            continue;
        }
        fire_fs_test_hook(TestHookPoint::AfterRecoveryGuardRemoval);
        if relative_file_matches_handle(root_handle, quarantine, &lease) {
            remove_non_directory_from_handle(root_handle, root, quarantine)?;
        }
    }
    for name in entries {
        let Some(name_text) = name.to_str() else {
            continue;
        };
        let Some(id) = exact_server_id(name_text) else {
            continue;
        };
        let lease_name = OsString::from(format!("server-{id}.lease"));
        if lease_open_fault == Some(root.join(&lease_name).as_path()) {
            continue;
        }
        let Ok(process_handle) = open_directory_from_handle(root_handle, &name) else {
            continue;
        };
        let lease_fd = match rustix::fs::openat(
            root_handle,
            Path::new(&lease_name),
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(_) => continue,
        };
        let lease = File::from(lease_fd);
        if !lease.metadata().is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        match FileExt::try_lock(&lease) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock | fs4::TryLockError::Error(_)) => continue,
        }
        if remove_owned_contents_from_handle(&process_handle, Path::new(".")).is_err() {
            continue;
        }
        drop(process_handle);
        if remove_directory_from_handle(root_handle, root, &name).is_err() {
            continue;
        }
        remove_locked_lease_from_handle(root_handle, id, root, &lease_name, &lease)?;
        drop(lease);
    }
    Ok(())
}

#[cfg(unix)]
fn relative_file_matches_handle(parent: &File, name: &OsStr, retained: &File) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(fd) = rustix::fs::openat(
        parent,
        Path::new(name),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
        return false;
    };
    let current = File::from(fd);
    let (Ok(current), Ok(retained)) = (current.metadata(), retained.metadata()) else {
        return false;
    };
    current.is_file() && retained.dev() == current.dev() && retained.ino() == current.ino()
}

#[cfg(unix)]
fn remove_locked_lease_from_handle(
    parent: &File,
    server_id: &str,
    _path: &Path,
    name: &OsStr,
    retained: &File,
) -> Result<(), ArtifactError> {
    let mut quarantine = None;
    remove_locked_lease_for_shutdown(parent, server_id, name, retained, &mut quarantine, || {
        fire_fs_test_hook(TestHookPoint::AfterLeaseQuarantine);
    })
}

#[cfg(unix)]
fn remove_locked_lease_for_shutdown(
    parent: &File,
    server_id: &str,
    name: &OsStr,
    retained: &File,
    quarantine: &mut Option<OsString>,
    after_quarantine: impl FnOnce(),
) -> Result<(), ArtifactError> {
    if quarantine.is_none() {
        if !relative_file_matches_handle(parent, name, retained) {
            return Err(ArtifactError::CleanupFailed(
                std::io::ErrorKind::InvalidInput,
            ));
        }
        let candidate = OsString::from(format!(
            "server-{server_id}.lease-cleanup-{}",
            new_server_id()
        ));
        rustix::fs::renameat(parent, Path::new(name), parent, Path::new(&candidate))
            .map_err(|error| ArtifactError::CleanupFailed(std::io::Error::from(error).kind()))?;
        *quarantine = Some(candidate);
        after_quarantine();
    }

    let quarantined = quarantine
        .as_ref()
        .ok_or(ArtifactError::InvalidOutputState)?;
    match rustix::fs::statat(
        parent,
        Path::new(name),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(_) => {
            return Err(ArtifactError::CleanupFailed(
                std::io::ErrorKind::AlreadyExists,
            ));
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => {
            return Err(ArtifactError::CleanupFailed(
                std::io::Error::from(error).kind(),
            ));
        }
    }
    if !relative_file_matches_handle(parent, quarantined, retained) {
        return Err(ArtifactError::CleanupFailed(
            std::io::ErrorKind::InvalidInput,
        ));
    }
    remove_non_directory_from_handle(parent, Path::new("."), quarantined)?;
    *quarantine = None;
    Ok(())
}

#[cfg(windows)]
fn remove_locked_lease_from_handle(
    parent: &File,
    _path: &Path,
    name: &std::ffi::OsStr,
    retained: &File,
) -> Result<(), ArtifactError> {
    let current = glass_windows::open_file_child(parent, name).map_err(map_host_cleanup)?;
    if !glass_windows::same_file_object(retained, &current).unwrap_or(false) {
        return Err(ArtifactError::CleanupFailed(
            std::io::ErrorKind::InvalidInput,
        ));
    }
    glass_windows::remove_by_handle(retained).map_err(map_host_cleanup)
}

#[cfg(windows)]
fn scavenge_root_from_handle(
    root: &Path,
    root_handle: &File,
    lease_open_fault: Option<&Path>,
) -> Result<(), ArtifactError> {
    let entries = glass_windows::directory_entry_records(root_handle).map_err(map_host_cleanup)?;
    fire_fs_test_hook(TestHookPoint::AfterScavengeEnumeration);
    for process in &entries {
        let Some(name_text) = process.name().to_str() else {
            continue;
        };
        let Some(id) = exact_server_id(name_text) else {
            continue;
        };
        let lease_name = std::ffi::OsString::from(format!("server-{id}.lease"));
        if lease_open_fault == Some(root.join(&lease_name).as_path()) {
            continue;
        }
        let Ok(process) = glass_windows::open_directory_entry(root_handle, process) else {
            continue;
        };
        if !process
            .metadata()
            .is_ok_and(|metadata| metadata.is_dir() && !is_reparse_point(&metadata))
        {
            continue;
        }
        let Some(lease) = entries.iter().find(|entry| entry.name() == lease_name) else {
            continue;
        };
        let Ok(lease) = glass_windows::open_directory_entry(root_handle, lease) else {
            continue;
        };
        if !lease
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && !is_reparse_point(&metadata))
        {
            continue;
        }
        match FileExt::try_lock(&lease) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock | fs4::TryLockError::Error(_)) => continue,
        }
        if remove_owned_contents_from_handle(&process, Path::new(".")).is_err() {
            continue;
        }
        if glass_windows::remove_by_handle(&process).is_err() {
            continue;
        }
        glass_windows::remove_by_handle(&lease).map_err(map_host_cleanup)?;
    }
    Ok(())
}

fn exact_server_id(name: &str) -> Option<&str> {
    let id = name.strip_prefix("server-")?;
    (id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(id)
}

fn exact_quarantine_id(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("server-")?;
    let (id, nonce) = rest.split_once(".lease-cleanup-")?;
    (id.len() == 32
        && nonce.len() == 32
        && id
            .bytes()
            .chain(nonce.bytes())
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(id)
}

fn safe_directory_metadata(metadata: &fs::Metadata) -> bool {
    metadata.is_dir() && !metadata.file_type().is_symlink() && !is_reparse_point(metadata)
}

fn safe_file_metadata(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink() && !is_reparse_point(metadata)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn open_existing_lease_no_follow(path: &Path) -> std::io::Result<File> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let file = File::from(fd);
    if !safe_file_metadata(&file.metadata()?) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_existing_lease_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0x0020_0000)
        .open(path)?;
    if !safe_file_metadata(&file.metadata()?) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    Ok(file)
}

#[cfg(unix)]
fn lease_handle_matches_path(handle: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(retained) = handle.metadata() else {
        return false;
    };
    let Ok(current) = fs::symlink_metadata(path) else {
        return false;
    };
    safe_file_metadata(&current)
        && retained.dev() == current.dev()
        && retained.ino() == current.ino()
}

#[cfg(windows)]
fn lease_handle_matches_path(handle: &File, path: &Path) -> bool {
    glass_windows::file_matches_path_no_reparse(handle, path).unwrap_or(false)
}

pub(crate) fn classify_uri<'a>(
    uri: &'a str,
    server_id: &str,
) -> Result<&'a str, ArtifactReadError> {
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
