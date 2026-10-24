//! Normal-owner access to worker-owned objects in an authorized writable tree.

use std::ffi::c_int;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use glass_core::Deadline;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, open, openat, statat};
use serde::{Deserialize, Serialize};

use super::{
    Acl, DIRECTORY_INHERIT, EXECUTE, Entry, Exclusions, FILE_INHERIT, FileIdentity, READ, Sharing,
    Tag, WRITE, WRITE_SECURITY, descriptor_capacity, mbr_uid_to_uuid,
};

// Avoid counting metadata observation as a content reader; ordinary open permissions still apply.
const EVENT_ONLY: OFlags = OFlags::from_bits_retain(0x8000);

unsafe extern "C" {
    fn glass_output_watch_start(
        path: *const std::ffi::c_char,
        context: *mut std::ffi::c_void,
        callback: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *const *const std::ffi::c_char,
            *const u32,
            usize,
        ),
        out: *mut *mut std::ffi::c_void,
    ) -> c_int;
    fn glass_output_watch_flush(handle: *mut std::ffi::c_void, timeout_ns: u64) -> c_int;
    fn glass_output_watch_stop(handle: *mut std::ffi::c_void);
}

/// Authority captured by the normal caller before dispatching private workspace work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputScope {
    owner: u32,
    worker: u32,
    owner_principal: [u8; 16],
    worker_principal: [u8; 16],
    path: PathBuf,
    identity: FileIdentity,
    protected_paths: Vec<PathBuf>,
    protected_identities: Vec<FileIdentity>,
    inspection: Vec<InspectionDirectory>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectionDirectory {
    identity: FileIdentity,
    readable: bool,
}

impl OutputScope {
    /// All writable roots in one lease must retain the same protected-object boundary.
    pub fn same_protection(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.worker == other.worker
            && self.owner_principal == other.owner_principal
            && self.worker_principal == other.worker_principal
            && self.protected_paths == other.protected_paths
            && self.protected_identities == other.protected_identities
    }
    /// Private executors need root authority, not the normal actor's inspection capabilities.
    pub fn for_worker(&self) -> Self {
        let mut scope = self.clone();
        scope.inspection.clear();
        scope
    }
    /// Capture one already-authorized writable root and the existing protected nodes.
    /// The caller must have selected this root for read/write access, not search-only access.
    pub fn capture(root: &Path, worker: u32, protected: &[PathBuf]) -> io::Result<Self> {
        let sharing = Sharing::new(worker)?;
        let path = root.canonicalize()?;
        let protected_paths = protected
            .iter()
            .map(|path| path.canonicalize())
            .collect::<io::Result<Vec<_>>>()?;
        if protected_paths
            .iter()
            .any(|protected| path.starts_with(protected))
        {
            return Err(refused("output root is protected"));
        }
        let file = File::from(open(
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let identity = FileIdentity::read(&file)?;
        let owner = rustix::process::geteuid().as_raw();
        if ![owner, worker].contains(&identity.owner) {
            return Err(refused("output root belongs to another account"));
        }
        let exclusions = Exclusions::new(&protected_paths, &path)?;
        let mut protected_identities = Vec::new();
        for file in &exclusions.files {
            let metadata = file.metadata()?;
            if exclusions.roots.contains(&(metadata.dev(), metadata.ino())) {
                protected_identities.push(FileIdentity::read(file)?);
            }
        }
        let mut inspection = Vec::new();
        for (index, ancestor) in path.ancestors().enumerate() {
            let file = File::open(ancestor)?;
            if file.metadata()?.uid() == owner {
                inspection.push(InspectionDirectory {
                    identity: FileIdentity::read(&file)?,
                    readable: index == 0,
                });
            }
        }
        Ok(Self {
            owner,
            worker,
            owner_principal: sharing.owner,
            worker_principal: sharing.worker,
            path,
            identity,
            protected_paths,
            protected_identities,
            inspection,
        })
    }

    /// Include directory access from this exact read/write preparation, excluding read-only inputs.
    pub fn capture_shared(
        root: &Path,
        worker: u32,
        protected: &[PathBuf],
        sharing: &Sharing,
    ) -> io::Result<Self> {
        let mut scope = Self::capture(root, worker, protected)?;
        if sharing.worker_uid != worker
            || sharing.owner != scope.owner_principal
            || sharing.worker != scope.worker_principal
        {
            return Err(refused("output preparation account binding changed"));
        }
        for node in sharing.nodes.values() {
            if node.file.metadata()?.uid() == scope.owner
                && node.file.metadata()?.is_dir()
                && node.owned.iter().any(|entry| {
                    entry.tag == Tag::Allow
                        && entry.principal == sharing.worker
                        && entry.permissions & WRITE != 0
                })
            {
                let identity = FileIdentity::read(&node.file)?;
                if let Some(directory) = scope
                    .inspection
                    .iter_mut()
                    .find(|directory| directory.identity == identity)
                {
                    directory.readable = true;
                } else {
                    scope.inspection.push(InspectionDirectory {
                        identity,
                        readable: true,
                    });
                }
            }
        }
        Ok(scope)
    }

    /// Validate stored structure without requiring an old, fully settled account to still exist.
    pub fn validate_binding(&self, owner: u32, worker: u32) -> io::Result<()> {
        if owner == 0
            || worker == 0
            || owner == worker
            || self.owner != owner
            || self.worker != worker
            || ![owner, worker].contains(&self.identity.owner)
            || !self.path.is_absolute()
            || self
                .path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || self.protected_paths.len() > 4096
            || self.protected_identities.len() > 4096
            || self.protected_paths.iter().any(|path| {
                !path.is_absolute()
                    || path
                        .components()
                        .any(|part| matches!(part, Component::ParentDir))
            })
            || self.inspection.len() > 1_000_000
            || self
                .inspection
                .iter()
                .any(|directory| directory.identity.owner != owner)
        {
            return Err(refused("output recovery account binding changed"));
        }
        Ok(())
    }

    /// Verify both current principals before any recovery effect, independent of path reachability.
    pub fn validate_accounts(&self, worker: u32) -> io::Result<()> {
        let owner = rustix::process::geteuid().as_raw();
        self.validate_binding(owner, worker)?;
        if owner != rustix::process::getuid().as_raw()
            || principal(owner)? != self.owner_principal
            || principal(worker)? != self.worker_principal
        {
            return Err(refused("output recovery account principals changed"));
        }
        Ok(())
    }

    /// Validate a retained guard against the original captured principal, without live lookup.
    pub fn validate_guard(&self, change: &super::PermissionChange) -> io::Result<()> {
        if change.additions.is_empty()
            || change.additions.iter().any(|entry| {
                entry.tag != Tag::Deny
                    || entry.principal != self.worker_principal
                    || entry.permissions != super::DELETE
                    || entry.flags != 0
            })
        {
            return Err(refused(
                "output checkpoint contains unowned permission evidence",
            ));
        }
        Ok(())
    }

    /// Validate durable authority as its ordinary normal-account owner, before inspection grants.
    pub fn validate_owner(&self, worker: u32) -> io::Result<()> {
        self.validate_accounts(worker)?;
        let file = File::from(open(
            &self.path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if FileIdentity::read(&file)? != self.identity {
            return Err(refused("output recovery root identity changed"));
        }
        Ok(())
    }

    /// After attributed writers stop, durably add only non-inheriting directory read/search access.
    /// The recorder owns revocation even after partial failure; it must settle grants before Closed.
    pub fn grant_inspection(
        &self,
        worker: u32,
        recorder: &mut dyn super::RecordChanges,
        deadline: Deadline,
    ) -> io::Result<usize> {
        check_deadline(deadline)?;
        self.validate_owner(worker)?;
        let mut added = 0;
        for directory in &self.inspection {
            check_deadline(deadline)?;
            if directory.identity.owner != self.owner {
                return Err(refused(
                    "output inspection directory belongs to another account",
                ));
            }
            let binding = super::PermissionChange {
                identity: directory.identity.clone(),
                before: Vec::new(),
                additions: Vec::new(),
            };
            let resolved = match binding.resolved_file() {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => continue,
                Err(error) => return Err(error),
            };
            let Some(file) = resolved else {
                continue;
            };
            if !file.metadata()?.is_dir() {
                return Err(refused("output inspection object is not a directory"));
            }
            let entry = Entry {
                tag: Tag::Allow,
                principal: self.worker_principal,
                permissions: EXECUTE | if directory.readable { READ } else { 0 },
                flags: 0,
            };
            let mut acl = Acl::read(&file)?;
            let before = acl.entries()?;
            if before.iter().any(|(_, current)| current.covers(&entry)) {
                continue;
            }
            let change = super::PermissionChange {
                identity: directory.identity.clone(),
                before: before.into_iter().map(|(_, entry)| entry).collect(),
                additions: vec![entry.clone()],
            };
            recorder.prepared(&change)?;
            acl.append(&entry)?;
            acl.write(&file)?;
            if FileIdentity::read(&file)? != directory.identity {
                return Err(refused("output inspection directory changed"));
            }
            added += 1;
        }
        Ok(added)
    }

    fn excludes(&self, file: &File, path: &Path) -> io::Result<bool> {
        if self
            .protected_paths
            .iter()
            .any(|protected| path.starts_with(protected))
        {
            return Ok(true);
        }
        let identity = FileIdentity::read(file)?;
        Ok(self
            .protected_identities
            .iter()
            .any(|protected| same_object(protected, &identity)))
    }
}

fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn check_deadline(deadline: Deadline) -> io::Result<()> {
    if deadline.has_passed() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "output access deadline elapsed",
        ))
    } else {
        Ok(())
    }
}

/// Runs as the private file creator; the transport must authenticate the receiving owner.
pub struct OutputAccess {
    owner: u32,
    worker: u32,
    principal: [u8; 16],
    worker_principal: [u8; 16],
}

fn principal(uid: u32) -> io::Result<[u8; 16]> {
    let mut result = [0; 16];
    // SAFETY: membership writes exactly one UUID into this live buffer.
    let code = unsafe { mbr_uid_to_uuid(uid, result.as_mut_ptr()) };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    Ok(result)
}

fn same_object(left: &FileIdentity, right: &FileIdentity) -> bool {
    left.device == right.device
        && left.inode == right.inode
        && left.birth_seconds == right.birth_seconds
        && left.birth_nanoseconds == right.birth_nanoseconds
        && left.fsid == right.fsid
}

impl OutputAccess {
    /// Select only the authenticated normal account. No root authority is used.
    pub fn new(owner: u32) -> io::Result<Self> {
        let worker = rustix::process::geteuid().as_raw();
        if worker == 0
            || owner == 0
            || owner == worker
            || rustix::process::getuid().as_raw() != worker
        {
            return Err(refused(
                "output access requires different ordinary owner and worker accounts",
            ));
        }
        Ok(Self {
            owner,
            worker,
            principal: principal(owner)?,
            worker_principal: principal(worker)?,
        })
    }

    fn denied(&self, entry: &Entry, permissions: u32) -> io::Result<bool> {
        if entry.tag != Tag::Deny
            || entry.flags & super::ONLY_INHERIT != 0
            || entry.permissions & permissions == 0
        {
            return Ok(false);
        }
        if entry.principal == self.principal {
            return Ok(true);
        }
        unsafe extern "C" {
            fn mbr_uuid_to_id(uuid: *const u8, id: *mut u32, kind: *mut c_int) -> c_int;
            fn mbr_check_membership(user: *const u8, group: *const u8, member: *mut c_int)
            -> c_int;
        }
        let (mut id, mut kind) = (0, 0);
        // SAFETY: UUID and both scalar output buffers stay live for this call.
        let code = unsafe { mbr_uuid_to_id(entry.principal.as_ptr(), &mut id, &mut kind) };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        if kind == 0 {
            return Ok(false);
        } // ID_TYPE_UID: a different user's entry.
        if kind != 1 {
            return Err(refused("output denial principal cannot be resolved"));
        }
        let mut member = 0;
        // SAFETY: both UUIDs and the scalar output stay live throughout the membership query.
        let code = unsafe {
            mbr_check_membership(
                self.principal.as_ptr(),
                entry.principal.as_ptr(),
                &mut member,
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok(member != 0)
    }

    fn repair_node(&self, file: &File, deadline: Deadline) -> io::Result<bool> {
        check_deadline(deadline)?;
        let metadata = file.metadata()?;
        if metadata.uid() != self.worker || (!metadata.is_dir() && !metadata.is_file()) {
            return Ok(false);
        }
        let directory = metadata.is_dir();
        let requested = Entry {
            tag: Tag::Allow,
            principal: self.principal,
            permissions: READ
                | WRITE
                | WRITE_SECURITY
                | if directory || metadata.mode() & 0o111 != 0 {
                    EXECUTE
                } else {
                    0
                },
            flags: if directory {
                FILE_INHERIT | DIRECTORY_INHERIT
            } else {
                0
            },
        };
        let mut acl = Acl::read(file)?;
        let existing = acl.entries()?;
        // Existing explicit delete restrictions remain; ordinary edits and ACL management must work.
        let required = requested.permissions & !super::DELETE;
        for (_, entry) in &existing {
            if self.denied(entry, required)? {
                return Err(refused("an existing output ACL denies normal-owner access"));
            }
        }
        if existing.iter().any(|(_, entry)| entry.covers(&requested)) {
            return Ok(false);
        }
        let before = FileIdentity::read(file)?;
        check_deadline(deadline)?;
        acl.append(&requested)?;
        acl.write(file)?;
        if FileIdentity::read(file)? != before {
            return Err(io::Error::other(
                "output identity changed during access repair",
            ));
        }
        Ok(true)
    }

    /// Add only missing normal-owner ACLs; preserve inode, content, mode and unrelated ACLs.
    /// A partial failure remains an error and must not be reported as completed workspace cleanup.
    pub fn repair(&self, scope: &OutputScope, deadline: Deadline) -> io::Result<usize> {
        let root = self.open_root(scope, deadline)?;
        self.repair_tree(scope, root, scope.path.clone(), deadline)
    }

    fn open_root(&self, scope: &OutputScope, deadline: Deadline) -> io::Result<File> {
        check_deadline(deadline)?;
        if scope.owner != self.owner
            || scope.worker != self.worker
            || scope.owner_principal != self.principal
            || scope.worker_principal != self.worker_principal
            || principal(self.owner)? != self.principal
            || principal(self.worker)? != self.worker_principal
            || !scope.path.is_absolute()
            || scope
                .path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || ![self.owner, self.worker].contains(&scope.identity.owner)
            || scope.protected_paths.len() > 4096
            || scope.protected_identities.len() > 4096
        {
            return Err(refused(
                "output scope does not match the authenticated accounts",
            ));
        }
        let root = File::from(open(
            &scope.path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if FileIdentity::read(&root)? != scope.identity {
            return Err(refused("output root identity changed"));
        }
        if scope.excludes(&root, &scope.path)? {
            return Err(refused("output root is protected"));
        }
        Ok(root)
    }

    fn open_child(
        &self,
        scope: &OutputScope,
        parent: &File,
        name: &std::ffi::OsStr,
        path: &Path,
    ) -> io::Result<Option<File>> {
        if scope
            .protected_paths
            .iter()
            .any(|protected| path.starts_with(protected))
        {
            return Ok(None);
        }
        let metadata = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let kind = FileType::from_raw_mode(metadata.st_mode);
        if !matches!(kind, FileType::Directory | FileType::RegularFile)
            || (kind == FileType::RegularFile && metadata.st_uid != self.worker)
        {
            return Ok(None);
        }
        if metadata.st_dev as u64 != scope.identity.device {
            return Err(refused("output traversal crossed a filesystem"));
        }
        let flags = OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC
            | if kind == FileType::Directory {
                OFlags::RDONLY | OFlags::DIRECTORY
            } else {
                EVENT_ONLY
            };
        let child = match descriptor_capacity(|| Ok(openat(parent, name, flags, Mode::empty())?)) {
            Ok(file) => File::from(file),
            Err(error)
                if [
                    Some(rustix::io::Errno::NOENT.raw_os_error()),
                    Some(rustix::io::Errno::LOOP.raw_os_error()),
                ]
                .contains(&error.raw_os_error()) =>
            {
                return Ok(None);
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("opening output {}: {error}", path.display()),
                ));
            }
        };
        let current = child.metadata()?;
        if current.dev() != metadata.st_dev as u64 || current.ino() != metadata.st_ino {
            return Err(io::Error::other("output entry changed during traversal"));
        }
        if scope.excludes(&child, path)? {
            return Ok(None);
        }
        Ok(Some(child))
    }

    /// Reconcile an observed subtree through pinned, no-follow components of the original root.
    pub fn repair_changed(
        &self,
        scope: &OutputScope,
        path: &Path,
        deadline: Deadline,
    ) -> io::Result<usize> {
        let mut file = self.open_root(scope, deadline)?;
        let relative = path
            .strip_prefix(&scope.path)
            .map_err(|_| refused("changed output is outside its authorized root"))?;
        let components: Vec<_> = relative.components().collect();
        if components
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(refused("changed output path is not normalized"));
        }
        let mut current = scope.path.clone();
        let mut repaired = 0;
        for component in components {
            check_deadline(deadline)?;
            let Component::Normal(name) = component else {
                return Err(refused("changed output path is not normalized"));
            };
            if !file.metadata()?.is_dir() {
                return Ok(repaired);
            }
            current.push(name);
            let Some(child) = self.open_child(scope, &file, name, &current)? else {
                return Ok(repaired);
            };
            repaired += usize::from(self.repair_node(&child, deadline)?);
            file = child;
        }
        if file.metadata()?.is_dir() {
            self.repair_tree(scope, file, current, deadline)
                .map(|count| count + repaired)
        } else {
            self.repair_node(&file, deadline)
                .map(|changed| repaired + usize::from(changed))
        }
    }

    fn repair_tree(
        &self,
        scope: &OutputScope,
        root: File,
        path: PathBuf,
        deadline: Deadline,
    ) -> io::Result<usize> {
        let mut repaired = usize::from(self.repair_node(&root, deadline)?);
        let entries = descriptor_capacity(|| Ok(Dir::read_from(&root)?))?;
        let mut stack = vec![(root, entries, path)];
        while let Some((parent, entries, path)) = stack.last_mut() {
            check_deadline(deadline)?;
            let Some(entry) = entries.next() else {
                stack.pop();
                continue;
            };
            let entry = entry?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let child_path = path.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
            let Some(child) = self.open_child(
                scope,
                parent,
                std::ffi::OsStr::from_bytes(name.to_bytes()),
                &child_path,
            )?
            else {
                continue;
            };
            repaired += usize::from(self.repair_node(&child, deadline)?);
            if child.metadata()?.is_dir() {
                let entries = descriptor_capacity(|| Ok(Dir::read_from(&child)?))?;
                stack.push((child, entries, child_path));
            }
        }
        Ok(repaired)
    }
}

struct Observation {
    scope: OutputScope,
    state: Mutex<ObservationState>,
    generation: AtomicU64,
}

struct ObservationState {
    access: Arc<Mutex<OutputAccess>>,
    failure: Option<String>,
}

impl Observation {
    fn lock(&self, deadline: Deadline) -> io::Result<std::sync::MutexGuard<'_, ObservationState>> {
        timed_lock(&self.state, deadline)
    }

    fn changed(&self, paths: &[PathBuf], full_scan: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let deadline = Deadline::from_millis(5000);
        let result = (|| {
            let access = timed_lock(&state.access, deadline)?;
            if full_scan {
                access.repair(&self.scope, deadline).map(|_| ())
            } else {
                paths.iter().try_for_each(|path| {
                    access
                        .repair_changed(&self.scope, path, deadline)
                        .map(|_| ())
                })
            }
        })();
        if let Err(error) = result {
            state.failure = Some(error.to_string());
        }
    }
}

fn timed_lock<T>(state: &Mutex<T>, deadline: Deadline) -> io::Result<std::sync::MutexGuard<'_, T>> {
    loop {
        check_deadline(deadline)?;
        match state.try_lock() {
            Ok(state) => return Ok(state),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("output access state is poisoned"));
            }
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(
                deadline
                    .remaining()
                    .unwrap_or(std::time::Duration::from_millis(5))
                    .min(std::time::Duration::from_millis(5)),
            ),
        }
    }
}

unsafe extern "C" fn changed(
    context: *mut std::ffi::c_void,
    paths: *const *const std::ffi::c_char,
    flags: *const u32,
    count: usize,
) {
    // SAFETY: the native stream borrows this pinned box and both arrays only for this callback.
    let observation = unsafe { &*context.cast::<Observation>() };
    observation.generation.fetch_add(1, Ordering::AcqRel);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if count == 0 {
            return;
        }
        if count > 4096 {
            observation.changed(&[], true);
            return;
        }
        // SAFETY: FSEvents supplies count initialized flags and NUL-terminated path pointers.
        let flags = unsafe { std::slice::from_raw_parts(flags, count) };
        if flags.iter().any(|flags| flags & (0xff | 0x800) != 0) {
            // Dropped/root events and renames need a scan; rename notifications may lack a pair.
            observation.changed(&[], true);
            return;
        }
        // SAFETY: the callback's native path array has the same count and lifetime as flags.
        let paths = unsafe { std::slice::from_raw_parts(paths, count) };
        let paths: std::collections::BTreeSet<_> = paths
            .iter()
            .map(|path| {
                // SAFETY: FSEvents supplies a valid, NUL-terminated filesystem path for each event.
                let bytes = unsafe { std::ffi::CStr::from_ptr(*path) }.to_bytes();
                PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
            })
            .collect();
        observation.changed(&paths.into_iter().collect::<Vec<_>>(), false);
    }));
    if result.is_err() {
        observation
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failure = Some("output observation panicked".into());
    }
}

/// Observe atomic replacements continuously within one authenticated writable root.
/// Drop stops and drains callbacks before releasing their borrowed state.
pub struct OutputMonitor {
    handle: NonNull<std::ffi::c_void>,
    observation: Box<Observation>,
}

// SAFETY: the native handle uses a serial dispatch queue; exclusive Rust ownership serializes
// flush/drop, and callback state is immutable apart from the mutex-protected failure.
unsafe impl Send for OutputMonitor {}

impl OutputMonitor {
    /// Arm observation before scanning so namespace changes during the initial scan are retained.
    pub fn new(scope: OutputScope, owner: u32, deadline: Deadline) -> io::Result<Self> {
        Self::with_access(
            scope,
            Arc::new(Mutex::new(OutputAccess::new(owner)?)),
            deadline,
        )
    }

    /// Serialize repair across overlapping roots and hard-link aliases in one broker.
    pub fn with_access(
        scope: OutputScope,
        access: Arc<Mutex<OutputAccess>>,
        deadline: Deadline,
    ) -> io::Result<Self> {
        timed_lock(&access, deadline)?.open_root(&scope, deadline)?;
        let path = std::ffi::CString::new(scope.path.as_os_str().as_bytes())?;
        let mut observation = Box::new(Observation {
            scope,
            state: Mutex::new(ObservationState {
                access,
                failure: None,
            }),
            generation: AtomicU64::new(0),
        });
        let mut handle = std::ptr::null_mut();
        // SAFETY: the box stays pinned until after native stop/drain; the path is copied at start.
        let code = unsafe {
            glass_output_watch_start(
                path.as_ptr(),
                (&mut *observation as *mut Observation).cast(),
                changed,
                &mut handle,
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        let monitor = Self {
            handle: NonNull::new(handle)
                .ok_or_else(|| io::Error::other("output observation returned no handle"))?,
            observation,
        };
        monitor.reconcile(deadline)?;
        Ok(monitor)
    }

    /// Drain delivered events; retry a failed event with a full scan before clearing its error.
    pub fn flush(&self, deadline: Deadline) -> io::Result<()> {
        self.flush_with(deadline, |access, scope, deadline| {
            access.repair(scope, deadline)
        })
    }

    fn flush_with(
        &self,
        deadline: Deadline,
        mut repair: impl FnMut(&OutputAccess, &OutputScope, Deadline) -> io::Result<usize>,
    ) -> io::Result<()> {
        loop {
            check_deadline(deadline)?;
            let timeout = deadline
                .remaining()
                .unwrap_or(std::time::Duration::from_secs(5))
                .as_nanos()
                .min(u64::MAX as u128) as u64;
            // SAFETY: the sole owner calls outside the callback queue; a timed-out barrier retains
            // native state for retry, and Drop drains it before releasing the callback box.
            let code = unsafe { glass_output_watch_flush(self.handle.as_ptr(), timeout) };
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code));
            }
            let generation = self.observation.generation.load(Ordering::Acquire);
            let mut state = self.observation.lock(deadline)?;
            if state.failure.is_none() {
                if generation == self.observation.generation.load(Ordering::Acquire) {
                    return Ok(());
                }
                continue;
            }
            {
                let access = timed_lock(&state.access, deadline)?;
                repair(&access, &self.observation.scope, deadline)?;
            }
            state.failure = None;
            // A save during recovery can queue behind the state lock. Drain again after releasing it.
        }
    }

    /// Complete an explicit full scan. Final cleanup must stop attributed writers first;
    /// event flushing alone cannot include writes that fseventsd has not published yet.
    pub fn reconcile(&self, deadline: Deadline) -> io::Result<()> {
        self.flush(deadline)?;
        {
            let state = self.observation.lock(deadline)?;
            timed_lock(&state.access, deadline)?.repair(&self.observation.scope, deadline)?;
        }
        self.flush(deadline)
    }
}

impl Drop for OutputMonitor {
    fn drop(&mut self) {
        // SAFETY: stop drains the queue before this instance releases the callback's pinned box.
        unsafe { glass_output_watch_stop(self.handle.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture(path: &Path) -> (OutputAccess, OutputScope) {
        let access = OutputAccess::new(u32::MAX - 1).unwrap();
        let file = File::open(path).unwrap();
        let scope = OutputScope {
            owner: access.owner,
            worker: access.worker,
            owner_principal: access.principal,
            worker_principal: access.worker_principal,
            path: path.canonicalize().unwrap(),
            identity: FileIdentity::read(&file).unwrap(),
            protected_paths: Vec::new(),
            protected_identities: Vec::new(),
            inspection: Vec::new(),
        };
        (access, scope)
    }

    fn entries(path: &Path) -> Vec<Entry> {
        let file = File::from(open(path, EVENT_ONLY | OFlags::NOFOLLOW, Mode::empty()).unwrap());
        Acl::read(&file)
            .unwrap()
            .entries()
            .unwrap()
            .into_iter()
            .map(|(_, entry)| entry)
            .collect()
    }

    #[test]
    fn atomic_replacement_keeps_its_inode_mode_data_and_existing_acl() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let target = project.join("saved");
        std::fs::write(&target, "first").unwrap();
        let (access, scope) = fixture(&project);
        access.repair(&scope, Deadline::from_millis(3000)).unwrap();
        let incoming = root.path().join("atomic");
        std::fs::write(&incoming, "second").unwrap();
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = FileIdentity::read(&File::open(&incoming).unwrap()).unwrap();
        let original = entries(&incoming);
        std::fs::rename(&incoming, &target).unwrap();
        assert!(
            !entries(&target)
                .iter()
                .any(|entry| entry.principal == access.principal)
        );
        assert_eq!(
            access.repair(&scope, Deadline::from_millis(3000)).unwrap(),
            1
        );
        assert_eq!(
            FileIdentity::read(&File::open(&target).unwrap()).unwrap(),
            before
        );
        assert_eq!(target.metadata().unwrap().mode() & 0o7777, 0o600);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
        let after = entries(&target);
        assert!(after.starts_with(&original));
        assert!(after.iter().any(|entry| entry.principal == access.principal
            && entry.permissions & (READ | WRITE | WRITE_SECURITY)
                == READ | WRITE | WRITE_SECURITY));
        assert_eq!(
            access.repair(&scope, Deadline::from_millis(3000)).unwrap(),
            0
        );
    }

    #[test]
    fn protected_nodes_and_outgoing_symlinks_are_untouched() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        let protected = project.join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        let secret = protected.join("secret");
        std::fs::write(&secret, "protected").unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, "outside").unwrap();
        std::os::unix::fs::symlink(&outside, project.join("alias")).unwrap();
        let (access, mut scope) = fixture(&project);
        scope
            .protected_paths
            .push(protected.canonicalize().unwrap());
        scope
            .protected_identities
            .push(FileIdentity::read(&File::open(&protected).unwrap()).unwrap());
        let before = [entries(&protected), entries(&secret), entries(&outside)];
        access.repair(&scope, Deadline::from_millis(3000)).unwrap();
        assert_eq!(
            [entries(&protected), entries(&secret), entries(&outside)],
            before
        );
    }

    #[test]
    fn access_follows_an_authorized_object_through_its_hard_links() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, "outside").unwrap();
        std::fs::hard_link(&outside, project.join("alias")).unwrap();
        let (access, scope) = fixture(&project);
        access.repair(&scope, Deadline::from_millis(3000)).unwrap();
        assert_eq!(entries(&outside), entries(&project.join("alias")));
        assert!(
            entries(&outside)
                .iter()
                .any(|entry| entry.principal == access.principal)
        );
    }

    #[test]
    fn protected_hard_link_identity_is_excluded_after_an_owner_change() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let outside = root.path().join("protected");
        std::fs::write(&outside, "protected").unwrap();
        std::fs::hard_link(&outside, project.join("alias")).unwrap();
        let before = entries(&outside);
        let (access, mut scope) = fixture(&project);
        let mut protected = FileIdentity::read(&File::open(&outside).unwrap()).unwrap();
        protected.owner = access.owner;
        scope.protected_identities.push(protected);
        access.repair(&scope, Deadline::from_millis(3000)).unwrap();
        assert_eq!(entries(&outside), before);
    }

    #[test]
    fn replaced_roots_wrong_accounts_and_elapsed_deadlines_refuse() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let (access, mut scope) = fixture(&project);
        assert_eq!(
            access
                .repair(&scope, Deadline::from_millis(0))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        scope.worker = access.owner;
        assert!(access.repair(&scope, Deadline::from_millis(3000)).is_err());
        scope.worker = access.worker;
        std::fs::rename(&project, root.path().join("old")).unwrap();
        std::fs::create_dir(&project).unwrap();
        assert!(access.repair(&scope, Deadline::from_millis(3000)).is_err());
        assert!(entries(&project).is_empty());
    }

    #[test]
    fn changed_account_principals_refuse_before_any_grant() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let target = root.path().join("saved");
        std::fs::write(&target, "data").unwrap();
        let (access, scope) = fixture(root.path());
        for change_owner in [true, false] {
            let mut changed = scope.clone();
            if change_owner {
                changed.owner_principal[0] ^= 1;
            } else {
                changed.worker_principal[0] ^= 1;
            }
            assert!(
                access
                    .repair(&changed, Deadline::from_millis(3000))
                    .is_err()
            );
            assert!(entries(root.path()).is_empty());
            assert!(entries(&target).is_empty());
        }
    }

    #[test]
    fn a_renamed_descriptor_repairs_only_the_original_object() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let target = root.path().join("saved");
        std::fs::write(&target, "first").unwrap();
        let access = OutputAccess::new(u32::MAX - 1).unwrap();
        let authorized = File::open(&target).unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&target, &moved).unwrap();
        std::fs::write(&target, "replacement").unwrap();
        access
            .repair_node(&authorized, Deadline::from_millis(3000))
            .unwrap();
        assert!(
            entries(&moved)
                .iter()
                .any(|entry| entry.principal == access.principal)
        );
        assert!(entries(&target).is_empty());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "replacement");
    }

    #[test]
    fn changed_paths_repair_only_the_selected_object_and_refuse_escapes() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let target = project.join("saved");
        let untouched = project.join("other");
        let outside = root.path().join("outside");
        for path in [&target, &untouched, &outside] {
            std::fs::write(path, "data").unwrap();
        }
        std::os::unix::fs::symlink(root.path(), project.join("alias")).unwrap();
        let (access, scope) = fixture(&project);
        assert_eq!(
            access
                .repair_changed(&scope, &target, Deadline::from_millis(3000))
                .unwrap(),
            1
        );
        assert!(entries(&untouched).is_empty());
        assert!(
            access
                .repair_changed(&scope, &outside, Deadline::from_millis(3000))
                .is_err()
        );
        assert!(
            access
                .repair_changed(
                    &scope,
                    &project.join("../outside"),
                    Deadline::from_millis(3000)
                )
                .is_err()
        );
        assert_eq!(
            access
                .repair_changed(
                    &scope,
                    &project.join("alias/outside"),
                    Deadline::from_millis(3000)
                )
                .unwrap(),
            0
        );
        assert!(entries(&outside).is_empty());
    }

    fn move_from_another_process(source: &Path, target: &Path) {
        assert!(
            std::process::Command::new("/bin/mv")
                .arg(source)
                .arg(target)
                .status()
                .unwrap()
                .success()
        );
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Deadline::from_millis(5000);
        while !ready() {
            assert!(
                !deadline.has_passed(),
                "output observation did not complete"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn native_observation_repairs_external_renames_without_a_tool_reply() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let incoming = root.path().join("incoming");
        std::fs::create_dir(&incoming).unwrap();
        std::fs::write(incoming.join("saved"), "atomic output").unwrap();
        let (access, scope) = fixture(&project);
        let monitor = OutputMonitor::new(scope, access.owner, Deadline::from_millis(5000)).unwrap();
        let target = project.join("nested");
        move_from_another_process(&incoming, &target);
        wait_until(|| {
            entries(&target.join("saved"))
                .iter()
                .any(|entry| entry.principal == access.principal)
        });
        monitor.reconcile(Deadline::from_millis(5000)).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("saved")).unwrap(),
            "atomic output"
        );
        drop(monitor);
        let later = root.path().join("later");
        std::fs::write(&later, "after observation").unwrap();
        move_from_another_process(&later, &project.join("later"));
        assert!(entries(&project.join("later")).is_empty());
    }

    #[test]
    fn native_observation_failures_remain_visible_until_a_full_scan_succeeds() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let incoming = root.path().join("unreadable");
        std::fs::write(&incoming, "private").unwrap();
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (access, scope) = fixture(&project);
        let monitor = OutputMonitor::new(scope, access.owner, Deadline::from_millis(5000)).unwrap();
        let target = project.join("unreadable");
        move_from_another_process(&incoming, &target);
        wait_until(|| monitor.observation.state.lock().unwrap().failure.is_some());
        assert!(monitor.flush(Deadline::from_millis(5000)).is_err());
        assert_eq!(target.metadata().unwrap().mode() & 0o7777, 0o000);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        monitor.flush(Deadline::from_millis(5000)).unwrap();
        assert!(monitor.observation.state.lock().unwrap().failure.is_none());
        assert!(
            entries(&target)
                .iter()
                .any(|entry| entry.principal == access.principal)
        );
    }

    #[test]
    fn an_unpaired_rename_rescans_for_the_unreported_destination() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let target = root.path().join("unreported-new-name");
        std::fs::write(&target, "atomic save").unwrap();
        let (access, scope) = fixture(root.path());
        let principal = access.principal;
        let mut observation = Observation {
            scope,
            state: Mutex::new(ObservationState {
                access: Arc::new(Mutex::new(access)),
                failure: None,
            }),
            generation: AtomicU64::new(0),
        };
        let old =
            std::ffi::CString::new(root.path().join("vanished-old-name").as_os_str().as_bytes())
                .unwrap();
        let paths = [old.as_ptr()];
        let flags = [0x800];
        // SAFETY: this synthetic native callback borrows our live observation and initialized arrays.
        unsafe {
            changed(
                (&mut observation as *mut Observation).cast(),
                paths.as_ptr(),
                flags.as_ptr(),
                1,
            )
        };
        assert!(
            entries(&target)
                .iter()
                .any(|entry| entry.principal == principal)
        );
    }

    #[test]
    fn a_save_during_failure_recovery_is_drained_before_success() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let incoming = root.path().join("atomic");
        std::fs::write(&incoming, "replacement").unwrap();
        let (access, scope) = fixture(&project);
        let monitor = OutputMonitor::new(scope, access.owner, Deadline::from_millis(5000)).unwrap();
        monitor.observation.state.lock().unwrap().failure = Some("rescan required".into());
        let target = project.join("saved");
        monitor
            .flush_with(Deadline::from_millis(5000), |access, scope, deadline| {
                let count = access.repair(scope, deadline)?;
                let generation = monitor.observation.generation.load(Ordering::Acquire);
                move_from_another_process(&incoming, &target);
                wait_until(|| monitor.observation.generation.load(Ordering::Acquire) != generation);
                Ok(count)
            })
            .unwrap();
        assert!(
            entries(&target)
                .iter()
                .any(|entry| entry.principal == access.principal)
        );
    }

    #[test]
    fn a_blocked_callback_does_not_extend_the_native_barrier_deadline() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let incoming = root.path().join("atomic");
        std::fs::write(&incoming, "replacement").unwrap();
        let (access, scope) = fixture(&project);
        let monitor = OutputMonitor::new(scope, access.owner, Deadline::from_millis(5000)).unwrap();
        let held = monitor.observation.state.lock().unwrap();
        move_from_another_process(&incoming, &project.join("saved"));
        let started = std::time::Instant::now();
        assert_eq!(
            monitor.flush(Deadline::from_millis(50)).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(held);
        monitor.flush(Deadline::from_millis(5000)).unwrap();
        assert!(
            entries(&project.join("saved"))
                .iter()
                .any(|entry| entry.principal == access.principal)
        );
    }

    #[test]
    fn explicit_recipient_denials_are_preserved_and_reported() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let target = root.path().join("private");
        std::fs::write(&target, "private").unwrap();
        let (access, scope) = fixture(root.path());
        let file = File::open(&target).unwrap();
        let mut acl = Acl::read(&file).unwrap();
        acl.append(&Entry {
            tag: Tag::Deny,
            principal: access.principal,
            permissions: WRITE_SECURITY,
            flags: 0,
        })
        .unwrap();
        acl.write(&file).unwrap();
        let before = entries(&target);
        assert!(access.repair(&scope, Deadline::from_millis(3000)).is_err());
        assert_eq!(entries(&target), before);
    }

    #[test]
    fn unreadable_files_refuse_without_changing_their_mode() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let target = root.path().join("unreadable");
        std::fs::write(&target, "data").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (access, scope) = fixture(root.path());
        let error = access
            .repair(&scope, Deadline::from_millis(3000))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("opening output"));
        assert_eq!(target.metadata().unwrap().mode() & 0o7777, 0);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(entries(&target).is_empty());
    }
}
