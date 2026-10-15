//! Scoped ACL additions for a private worker and normal-owner access to its outputs.

use std::collections::BTreeMap;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use rustix::fs::{Dir, FileType, Mode, OFlags, open, openat};

const EXTENDED: u32 = 0x100;
const ALLOW: u32 = 1;
const EXECUTE: u32 = 1 << 3;
const WRITE_SECURITY: u32 = 1 << 12;
const FILE_INHERIT: u32 = 1 << 5;
const DIRECTORY_INHERIT: u32 = 1 << 6;
const LIMIT_INHERIT: u32 = 1 << 7;
const ONLY_INHERIT: u32 = 1 << 8;
const READ: u32 = (1 << 1) | (1 << 7) | (1 << 9) | (1 << 11);
const WRITE: u32 = (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 10);
const PERMISSIONS: &[u32] = &[
    1 << 1,
    1 << 2,
    1 << 3,
    1 << 4,
    1 << 5,
    1 << 6,
    1 << 7,
    1 << 8,
    1 << 9,
    1 << 10,
    1 << 11,
    1 << 12,
    1 << 13,
    1 << 20,
];
const FLAGS: &[u32] = &[1 << 4, 1 << 5, 1 << 6, 1 << 7, 1 << 8];

fn descriptor_capacity<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::MFILE.raw_os_error()) => {
                use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
                let limit = getrlimit(Resource::Nofile);
                let Some(current) = limit.current else {
                    return Err(error);
                };
                let next = current.saturating_mul(2).max(current.saturating_add(1));
                let next = limit.maximum.map_or(next, |maximum| next.min(maximum));
                if next <= current {
                    return Err(error);
                }
                // Only this connection process changes; all retained descriptors are CLOEXEC.
                setrlimit(
                    Resource::Nofile,
                    Rlimit {
                        current: Some(next),
                        maximum: limit.maximum,
                    },
                )?;
            }
            result => return result,
        }
    }
}

unsafe extern "C" {
    fn acl_get_fd_np(fd: c_int, kind: u32) -> *mut c_void;
    fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: u32) -> c_int;
    fn acl_init(count: c_int) -> *mut c_void;
    fn acl_free(value: *mut c_void) -> c_int;
    fn acl_get_entry(acl: *mut c_void, index: c_int, output: *mut *mut c_void) -> c_int;
    fn acl_create_entry(acl: *mut *mut c_void, output: *mut *mut c_void) -> c_int;
    fn acl_delete_entry(acl: *mut c_void, entry: *mut c_void) -> c_int;
    fn acl_get_tag_type(entry: *mut c_void, output: *mut u32) -> c_int;
    fn acl_set_tag_type(entry: *mut c_void, kind: u32) -> c_int;
    fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
    fn acl_set_qualifier(entry: *mut c_void, qualifier: *const c_void) -> c_int;
    fn acl_get_permset(entry: *mut c_void, output: *mut *mut c_void) -> c_int;
    fn acl_get_perm_np(permissions: *mut c_void, permission: u32) -> c_int;
    fn acl_add_perm(permissions: *mut c_void, permission: u32) -> c_int;
    fn acl_set_permset(entry: *mut c_void, permissions: *mut c_void) -> c_int;
    fn acl_get_flagset_np(entry: *mut c_void, output: *mut *mut c_void) -> c_int;
    fn acl_get_flag_np(flags: *mut c_void, flag: u32) -> c_int;
    fn acl_add_flag_np(flags: *mut c_void, flag: u32) -> c_int;
    fn acl_set_flagset_np(entry: *mut c_void, flags: *mut c_void) -> c_int;
    fn acl_valid(acl: *mut c_void) -> c_int;
    fn mbr_uid_to_uuid(uid: u32, uuid: *mut u8) -> c_int;
}

/// Permissions added to selected nodes; existing permissions are retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Traverse this directory without listing it or inheriting access below it.
    Search,
    /// Read the tree, including executable files and directory traversal.
    Read,
    /// Read/write the tree and keep normal-owner access to generated files.
    ReadWrite,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    principal: [u8; 16],
    permissions: u32,
    flags: u32,
}

/// A file identity retained before an ACL mutation, including its birth time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[repr(C)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
    birth_seconds: u64,
    birth_nanoseconds: u64,
    owner: u32,
    fsid: [i32; 2],
}

impl FileIdentity {
    fn read(file: &File) -> io::Result<Self> {
        unsafe extern "C" {
            fn glass_private_file_identity(fd: c_int, out: *mut FileIdentity) -> c_int;
        }
        let mut identity = Self::default();
        // SAFETY: the C helper fills one live repr(C) value from a borrowed descriptor.
        let code = unsafe { glass_private_file_identity(file.as_raw_fd(), &mut identity) };
        if code == 0 {
            Ok(identity)
        } else {
            Err(io::Error::from_raw_os_error(code))
        }
    }
}

/// Write-ahead evidence for exactly one additive ACL operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionChange {
    identity: FileIdentity,
    before: Vec<Entry>,
    additions: Vec<Entry>,
}

/// A sink must make preparation durable before returning; settlement is idempotent.
pub trait RecordChanges: Send {
    /// Persist an intended mutation before it is attempted.
    fn prepared(&mut self, change: &PermissionChange) -> io::Result<u64>;
    /// Persist that the mutation no longer needs recovery.
    fn settled(&mut self, identity: u64) -> io::Result<()>;
}

/// Shared recorder for independently rolled-back permission scopes.
pub type Recorder = Arc<Mutex<dyn RecordChanges>>;

/// Refuse a recovery store with an ACL that authorizes another principal.
pub fn require_owner_private(file: &File) -> io::Result<()> {
    let owner = rustix::process::geteuid().as_raw();
    let mut principal = [0; 16];
    // SAFETY: membership writes exactly one UUID into the writable buffer.
    let code = unsafe { mbr_uid_to_uuid(owner, principal.as_mut_ptr()) };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if file.metadata()?.uid() != owner
        || Acl::read(file)?
            .entries()?
            .iter()
            .any(|(_, entry)| entry.principal != principal && entry.permissions != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission journal ACL is not private",
        ));
    }
    Ok(())
}

impl PermissionChange {
    /// Recover the same filesystem object, including after a rename; never act on its replacement.
    /// An unlinked object with no pathname has no grant reachable by a later open.
    pub fn restore(&self, worker_uid: u32) -> io::Result<()> {
        let sharing = Sharing::new(worker_uid)?;
        if self.additions.is_empty()
            || self.additions.iter().any(|entry| {
                ![sharing.worker, sharing.owner].contains(&entry.principal)
                    || entry.permissions & !(READ | WRITE | EXECUTE | WRITE_SECURITY) != 0
                    || entry.flags & !(FILE_INHERIT | DIRECTORY_INHERIT) != 0
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "permission recovery evidence is not owned",
            ));
        }
        if let Some(file) = self.resolved_file()? {
            self.restore_file(&file)?;
        }
        Ok(())
    }

    fn resolved_file(&self) -> io::Result<Option<File>> {
        unsafe extern "C" {
            fn glass_private_resolve_file(
                identity: *const FileIdentity,
                out: *mut u8,
                size: usize,
            ) -> c_int;
        }
        let mut path = [0u8; 4096];
        // SAFETY: the helper reads the repr(C) identity and writes within this live buffer.
        let code =
            unsafe { glass_private_resolve_file(&self.identity, path.as_mut_ptr(), path.len()) };
        if code == 2 {
            return Ok(None);
        }
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        let end = path.iter().position(|&byte| byte == 0).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "recovered file path is unterminated",
            )
        })?;
        use std::os::unix::ffi::OsStrExt;
        let file = File::from(open(
            Path::new(std::ffi::OsStr::from_bytes(&path[..end])),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if FileIdentity::read(&file)? != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "permission recovery file identity changed",
            ));
        }
        Ok(Some(file))
    }

    fn restore_file(&self, file: &File) -> io::Result<()> {
        let acl = Acl::read(file)?;
        let mut changed = false;
        for owned in &self.additions {
            let baseline = self.before.iter().filter(|entry| *entry == owned).count();
            let current: Vec<_> = acl
                .entries()?
                .into_iter()
                .filter(|(_, entry)| entry == owned)
                .collect();
            if current.len() > baseline
                && let Some((entry, _)) = current.last()
            {
                // SAFETY: this descriptor belongs to the live ACL and is discarded after deletion.
                check(unsafe { acl_delete_entry(acl.0.as_ptr(), *entry) })?;
                changed = true;
            }
        }
        if changed {
            acl.write(file)?;
        }
        Ok(())
    }
}

impl Entry {
    fn covers(&self, requested: &Self) -> bool {
        self.principal == requested.principal
            && self.permissions & requested.permissions == requested.permissions
            && self.flags & ONLY_INHERIT == 0
            && self.flags & requested.flags == requested.flags
            && (requested.flags == 0 || self.flags & LIMIT_INHERIT == 0)
    }
}

struct Acl(NonNull<c_void>);

fn check(code: c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Acl {
    fn read(file: &File) -> io::Result<Self> {
        // SAFETY: the descriptor is borrowed from a live file; the result is caller-owned.
        let mut pointer = unsafe { acl_get_fd_np(file.as_raw_fd(), EXTENDED) };
        if pointer.is_null() && io::Error::last_os_error().raw_os_error() == Some(2) {
            // SAFETY: acl_init returns an independent, caller-owned empty ACL.
            pointer = unsafe { acl_init(4) };
        }
        NonNull::new(pointer)
            .map(Self)
            .ok_or_else(io::Error::last_os_error)
    }

    fn entries(&self) -> io::Result<Vec<(*mut c_void, Entry)>> {
        let mut result = Vec::new();
        let mut index = 0;
        loop {
            let mut entry = std::ptr::null_mut();
            // SAFETY: this ACL is live, and the output receives a descriptor borrowed from it.
            let code = unsafe { acl_get_entry(self.0.as_ptr(), index, &mut entry) };
            if code != 0 {
                // Darwin's index extension reports EINVAL after the last valid entry.
                return if io::Error::last_os_error().raw_os_error() == Some(22) {
                    Ok(result)
                } else {
                    Err(io::Error::last_os_error())
                };
            }
            if entry.is_null() {
                return Err(io::Error::other("ACL returned an empty entry"));
            }
            let mut tag = 0;
            // SAFETY: entry belongs to this live ACL, and tag is writable storage.
            check(unsafe { acl_get_tag_type(entry, &mut tag) })?;
            if tag == ALLOW {
                // SAFETY: the API returns a caller-owned 16-byte membership UUID for this entry.
                let qualifier = unsafe { acl_get_qualifier(entry) };
                if qualifier.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let mut principal = [0; 16];
                // SAFETY: qualifier points to the documented 16-byte UUID allocation.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        qualifier.cast::<u8>(),
                        principal.as_mut_ptr(),
                        16,
                    );
                    acl_free(qualifier);
                }
                let mut permissions = std::ptr::null_mut();
                let mut flags = std::ptr::null_mut();
                // SAFETY: both descriptors are borrowed from this live entry, with writable outputs.
                unsafe {
                    check(acl_get_permset(entry, &mut permissions))?;
                    check(acl_get_flagset_np(entry, &mut flags))?;
                }
                let mut permission_mask = 0;
                for &bit in PERMISSIONS {
                    // SAFETY: permissions is borrowed from the live entry; bit is an SDK permission.
                    let present = unsafe { acl_get_perm_np(permissions, bit) };
                    if present < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if present == 1 {
                        permission_mask |= bit;
                    }
                }
                let mut flag_mask = 0;
                for &bit in FLAGS {
                    // SAFETY: flags is borrowed from the live entry; bit is an SDK entry flag.
                    let present = unsafe { acl_get_flag_np(flags, bit) };
                    if present < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if present == 1 {
                        flag_mask |= bit;
                    }
                }
                result.push((
                    entry,
                    Entry {
                        principal,
                        permissions: permission_mask,
                        flags: flag_mask,
                    },
                ));
            }
            index += 1;
        }
    }

    fn append(&mut self, value: &Entry) -> io::Result<()> {
        let mut pointer = self.0.as_ptr();
        let mut entry = std::ptr::null_mut();
        // SAFETY: the API may reallocate this owned ACL and fills a descriptor into its new storage.
        let code = unsafe { acl_create_entry(&mut pointer, &mut entry) };
        self.0 = NonNull::new(pointer).ok_or_else(io::Error::last_os_error)?;
        check(code)?;
        let mut permissions = std::ptr::null_mut();
        let mut flags = std::ptr::null_mut();
        // SAFETY: entry belongs to the owned ACL; UUID storage stays live during the copying setter.
        unsafe {
            check(acl_set_tag_type(entry, ALLOW))?;
            check(acl_set_qualifier(entry, value.principal.as_ptr().cast()))?;
            check(acl_get_permset(entry, &mut permissions))?;
            check(acl_get_flagset_np(entry, &mut flags))?;
        }
        for &bit in PERMISSIONS {
            if value.permissions & bit != 0 {
                // SAFETY: permissions is live and bit is a supported SDK permission.
                check(unsafe { acl_add_perm(permissions, bit) })?;
            }
        }
        for &bit in FLAGS {
            if value.flags & bit != 0 {
                // SAFETY: flags is live and bit is a supported SDK entry flag.
                check(unsafe { acl_add_flag_np(flags, bit) })?;
            }
        }
        // SAFETY: both sets belong to this ACL entry and remain live throughout the setters.
        unsafe {
            check(acl_set_permset(entry, permissions))?;
            check(acl_set_flagset_np(entry, flags))?;
        }
        Ok(())
    }

    fn write(&self, file: &File) -> io::Result<()> {
        // SAFETY: the ACL and file descriptor remain live throughout validation and application.
        unsafe {
            check(acl_valid(self.0.as_ptr()))?;
            check(acl_set_fd_np(file.as_raw_fd(), self.0.as_ptr(), EXTENDED))
        }
    }
}

impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: this unique ACL allocation has not been freed or transferred.
        unsafe {
            acl_free(self.0.as_ptr());
        }
    }
}

struct Node {
    file: File,
    owned: Vec<Entry>,
    records: Vec<(u64, PermissionChange)>,
}

/// ACL additions owned by one worker connection. Call `restore` after its worker exits.
/// New output files retain inherited normal-owner access; this does not revoke their ownership.
pub struct Sharing {
    worker_uid: u32,
    worker: [u8; 16],
    owner: [u8; 16],
    nodes: BTreeMap<(u64, u64), Node>,
    recorder: Option<Recorder>,
}

impl Sharing {
    /// Resolve the selected account's current membership UUID as an ordinary caller.
    pub fn new(worker_uid: u32) -> io::Result<Self> {
        let owner_uid = rustix::process::geteuid().as_raw();
        if owner_uid == 0
            || owner_uid != rustix::process::getuid().as_raw()
            || worker_uid == 0
            || owner_uid == worker_uid
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace sharing requires its ordinary owner and a different worker account",
            ));
        }
        fn principal(uid: u32) -> io::Result<[u8; 16]> {
            let mut result = [0; 16];
            // SAFETY: membership copies exactly one UUID into the writable output buffer.
            let code = unsafe { mbr_uid_to_uuid(uid, result.as_mut_ptr()) };
            if code == 0 {
                Ok(result)
            } else {
                Err(io::Error::from_raw_os_error(code))
            }
        }
        Ok(Self {
            worker_uid,
            worker: principal(worker_uid)?,
            owner: principal(owner_uid)?,
            nodes: BTreeMap::new(),
            recorder: None,
        })
    }

    /// Record mutations before applying them, sharing the sink across per-app scopes.
    pub fn recorded(worker_uid: u32, recorder: Recorder) -> io::Result<Self> {
        let mut sharing = Self::new(worker_uid)?;
        sharing.recorder = Some(recorder);
        Ok(sharing)
    }

    /// Add search-only access to one directory. No descendant entry is inherited.
    pub fn search(&mut self, directory: &Path) -> io::Result<()> {
        let file = File::from(descriptor_capacity(|| {
            Ok(open(
                directory,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?)
        })?);
        self.grant(file, Access::Search, false)
    }

    /// Grant a selected tree, preserving hard-link identity and skipping outgoing symlinks.
    pub fn tree(&mut self, directory: &Path, access: Access) -> io::Result<()> {
        if access == Access::Search {
            return self.search(directory);
        }
        let canonical = directory.canonicalize()?;
        let file = File::from(descriptor_capacity(|| {
            Ok(open(
                &canonical,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?)
        })?);
        self.grant(file, access, true)
    }

    /// Share an explicitly selected file or directory, resolving the selected root's symlink.
    /// Descendant symlinks are still skipped; search-only access requires a directory.
    pub fn path(&mut self, path: &Path, access: Access) -> io::Result<()> {
        let canonical = path.canonicalize()?;
        let file = File::from(descriptor_capacity(|| {
            Ok(open(
                &canonical,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?)
        })?);
        let metadata = file.metadata()?;
        if (!metadata.is_dir() && !metadata.is_file())
            || (access == Access::Search && !metadata.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace access requires a regular file or directory",
            ));
        }
        self.grant(file, access, access != Access::Search)
    }

    fn grant(&mut self, file: File, access: Access, recursive: bool) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Ok(());
        }
        let identity = (metadata.dev(), metadata.ino());
        let directory = metadata.is_dir();
        let mut rights = if access == Access::Search { 0 } else { READ };
        if access == Access::ReadWrite {
            rights |= WRITE;
        }
        if directory || metadata.mode() & 0o111 != 0 {
            rights |= EXECUTE;
        }
        let inherited = if directory && access != Access::Search {
            FILE_INHERIT | DIRECTORY_INHERIT
        } else {
            0
        };
        let required_mode = if access == Access::Search { 0 } else { 0o400 }
            | if access == Access::ReadWrite {
                0o200
            } else {
                0
            }
            | if rights & EXECUTE != 0 { 0o100 } else { 0 };
        let worker_already_owns_access =
            metadata.uid() == self.worker_uid && metadata.mode() & required_mode == required_mode;
        let mut entries = Vec::new();
        if !worker_already_owns_access {
            entries.push(Entry {
                principal: self.worker,
                permissions: rights,
                flags: inherited,
            });
        }
        if directory && access == Access::ReadWrite {
            entries.push(Entry {
                principal: self.owner,
                permissions: READ | WRITE | EXECUTE | WRITE_SECURITY,
                flags: inherited,
            });
        }
        let mut acl = Acl::read(&file)?;
        let existing = acl.entries()?;
        let additions: Vec<Entry> = entries
            .into_iter()
            .filter(|entry| !existing.iter().any(|(_, current)| current.covers(entry)))
            .collect();
        if !additions.is_empty() {
            let change = PermissionChange {
                identity: FileIdentity::read(&file)?,
                before: existing.iter().map(|(_, entry)| entry.clone()).collect(),
                additions: additions.clone(),
            };
            let record = self
                .recorder
                .as_ref()
                .map(|recorder| {
                    recorder
                        .lock()
                        .map_err(|_| io::Error::other("permission recorder is poisoned"))?
                        .prepared(&change)
                })
                .transpose()?;
            let tracked_file = if self.nodes.contains_key(&identity) {
                None
            } else {
                Some(descriptor_capacity(|| file.try_clone())?)
            };
            if let Some(node) = self.nodes.get_mut(&identity) {
                node.owned.extend(additions.iter().cloned());
                if let Some(record) = record {
                    node.records.push((record, change));
                }
            } else {
                if let Some(file) = tracked_file {
                    self.nodes.insert(
                        identity,
                        Node {
                            file,
                            owned: additions.clone(),
                            records: record
                                .map(|record| vec![(record, change)])
                                .unwrap_or_default(),
                        },
                    );
                }
            }
            for entry in &additions {
                acl.append(entry)?;
            }
            acl.write(&file)?;
        }
        if recursive && directory {
            for entry in descriptor_capacity(|| Ok(Dir::read_from(&file)?))? {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                if !matches!(
                    entry.file_type(),
                    FileType::RegularFile | FileType::Directory | FileType::Unknown
                ) {
                    continue;
                }
                let opened = match descriptor_capacity(|| {
                    Ok(openat(
                        &file,
                        name,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?)
                }) {
                    Ok(file) => File::from(file),
                    Err(error)
                        if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                self.grant(opened, access, true)?;
            }
        }
        Ok(())
    }

    /// Remove only this connection's additions from the current ACLs, retaining user edits.
    /// Errors keep unresolved entries owned by this object for an explicit retry or inspection.
    pub fn restore(&mut self) -> io::Result<()> {
        let mut failure = None;
        for node in self.nodes.values_mut() {
            if node.owned.is_empty() {
                continue;
            }
            let result = (|| {
                if let Some(recorder) = &self.recorder {
                    for (record, change) in node.records.iter().rev() {
                        change.restore_file(&node.file)?;
                        recorder
                            .lock()
                            .map_err(|_| io::Error::other("permission recorder is poisoned"))?
                            .settled(*record)?;
                    }
                    node.records.clear();
                    node.owned.clear();
                    return Ok(());
                }
                let acl = Acl::read(&node.file)?;
                for owned in &node.owned {
                    let entry = acl
                        .entries()?
                        .into_iter()
                        .rev()
                        .find(|(_, entry)| entry == owned);
                    if let Some((entry, _)) = entry {
                        // SAFETY: entry is borrowed from this ACL and is discarded after deletion.
                        check(unsafe { acl_delete_entry(acl.0.as_ptr(), entry) })?;
                    }
                }
                acl.write(&node.file)?;
                node.owned.clear();
                Ok::<_, io::Error>(())
            })();
            if let Err(error) = result {
                failure = Some(error);
            }
        }
        self.nodes.retain(|_, node| !node.owned.is_empty());
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for Sharing {
    fn drop(&mut self) {
        if self.nodes.values().any(|node| !node.owned.is_empty()) {
            eprintln!(
                "glass: workspace ACL additions remain; worker exit and permission cleanup require verification"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn same_user_sharing() -> Sharing {
        let mut principal = [0; 16];
        // SAFETY: membership writes one UUID into the writable test buffer.
        assert_eq!(
            unsafe { mbr_uid_to_uuid(rustix::process::getuid().as_raw(), principal.as_mut_ptr()) },
            0
        );
        Sharing {
            worker_uid: u32::MAX,
            worker: principal,
            owner: principal,
            nodes: BTreeMap::new(),
            recorder: None,
        }
    }

    #[derive(Default)]
    struct Recorded {
        changes: Vec<PermissionChange>,
        settled: Vec<u64>,
        refuse: bool,
    }

    impl RecordChanges for Recorded {
        fn prepared(&mut self, change: &PermissionChange) -> io::Result<u64> {
            let file = change
                .resolved_file()?
                .ok_or_else(|| io::Error::other("test object disappeared"))?;
            assert_eq!(
                entries(&file),
                change.before,
                "journal callback must precede ACL changes"
            );
            if self.refuse {
                return Err(io::Error::other("injected journal failure"));
            }
            self.changes.push(change.clone());
            Ok(self.changes.len() as u64)
        }
        fn settled(&mut self, id: u64) -> io::Result<()> {
            self.settled.push(id);
            Ok(())
        }
    }

    #[test]
    fn journal_failure_prevents_the_permission_mutation() {
        let root = tempfile::tempdir().unwrap();
        let file = File::open(root.path()).unwrap();
        let before = entries(&file);
        let recorder = Arc::new(Mutex::new(Recorded {
            refuse: true,
            ..Default::default()
        }));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder);
        assert!(sharing.search(root.path()).is_err());
        assert_eq!(entries(&file), before);
    }

    #[test]
    fn recorded_scope_settles_only_after_restoring_its_grant() {
        let root = tempfile::tempdir().unwrap();
        let file = File::open(root.path()).unwrap();
        let before = entries(&file);
        let recorder = Arc::new(Mutex::new(Recorded::default()));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder.clone());
        sharing.search(root.path()).unwrap();
        assert!(recorder.lock().unwrap().settled.is_empty());
        sharing.restore().unwrap();
        assert_eq!(entries(&file), before);
        assert_eq!(recorder.lock().unwrap().settled, vec![1]);
    }

    #[test]
    fn renamed_file_is_recovered_by_identity_and_other_edits_are_retained() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::write(&source, "source").unwrap();
        let file = File::open(&source).unwrap();
        let before = entries(&file);
        let recorder = Arc::new(Mutex::new(Recorded::default()));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder.clone());
        sharing.path(&source, Access::Read).unwrap();
        let change = recorder.lock().unwrap().changes[0].clone();
        let edit = Entry {
            principal: sharing.owner,
            permissions: WRITE_SECURITY,
            flags: 0,
        };
        let mut changed = Acl::read(&file).unwrap();
        changed.append(&edit).unwrap();
        changed.write(&file).unwrap();
        std::fs::rename(&source, root.path().join("renamed")).unwrap();
        drop(sharing);
        let reopened = change.resolved_file().unwrap().unwrap();
        change.restore_file(&reopened).unwrap();
        change.restore_file(&reopened).unwrap();
        let mut expected = before;
        expected.push(edit);
        assert_eq!(entries(&reopened), expected);
    }

    #[test]
    fn replaced_path_does_not_revoke_permissions_on_its_new_file() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::write(&source, "source").unwrap();
        let recorder = Arc::new(Mutex::new(Recorded::default()));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder.clone());
        sharing.path(&source, Access::Read).unwrap();
        let change = recorder.lock().unwrap().changes[0].clone();
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, "replacement").unwrap();
        let replacement = File::open(&source).unwrap();
        let before = entries(&replacement);
        assert!(change.resolved_file().unwrap().is_none());
        assert_eq!(entries(&replacement), before);
        sharing.restore().unwrap();
    }

    #[test]
    fn changed_birth_time_refuses_inode_reuse() {
        let root = tempfile::tempdir().unwrap();
        let file = File::open(root.path()).unwrap();
        let recorder = Arc::new(Mutex::new(Recorded::default()));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder.clone());
        sharing.search(root.path()).unwrap();
        let mut change = recorder.lock().unwrap().changes[0].clone();
        change.identity.birth_nanoseconds ^= 1;
        let before = entries(&file);
        assert!(change.resolved_file().is_err());
        assert_eq!(entries(&file), before);
        sharing.restore().unwrap();
    }

    #[test]
    fn unavailable_volume_does_not_count_as_a_deleted_object() {
        let root = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Mutex::new(Recorded::default()));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder.clone());
        sharing.search(root.path()).unwrap();
        let mut change = recorder.lock().unwrap().changes[0].clone();
        change.identity.fsid = [-1, -1];
        assert!(change.resolved_file().is_err());
        sharing.restore().unwrap();
    }

    fn entries(file: &File) -> Vec<Entry> {
        Acl::read(file)
            .unwrap()
            .entries()
            .unwrap()
            .into_iter()
            .map(|(_, entry)| entry)
            .collect()
    }

    #[test]
    fn inherited_permissions_cover_access_without_rewriting_their_acl() {
        let requested = Entry {
            principal: [1; 16],
            permissions: READ | WRITE,
            flags: FILE_INHERIT | DIRECTORY_INHERIT,
        };
        let mut inherited = requested.clone();
        inherited.permissions |= EXECUTE | WRITE_SECURITY;
        inherited.flags |= 1 << 4;
        assert!(inherited.covers(&requested));
        inherited.flags |= LIMIT_INHERIT;
        assert!(!inherited.covers(&requested));
        inherited.flags = ONLY_INHERIT | requested.flags;
        assert!(!inherited.covers(&requested));
    }

    #[test]
    fn search_only_additions_restore_an_originally_empty_acl() {
        let root = tempfile::tempdir().unwrap();
        let file = File::open(root.path()).unwrap();
        let before = entries(&file);
        let mut sharing = same_user_sharing();
        sharing.search(root.path()).unwrap();
        assert!(
            entries(&file)
                .iter()
                .any(|entry| entry.permissions == EXECUTE)
        );
        sharing.restore().unwrap();
        assert_eq!(entries(&file), before);
    }

    #[test]
    fn worker_owned_atomic_outputs_need_no_new_acl() {
        let root = tempfile::tempdir().unwrap();
        let saved = root.path().join("saved-note");
        std::fs::write(&saved, "atomic application output").unwrap();
        let file = File::open(&saved).unwrap();
        let before = entries(&file);
        let mut sharing = same_user_sharing();
        sharing.worker_uid = rustix::process::getuid().as_raw();
        sharing.tree(root.path(), Access::ReadWrite).unwrap();
        assert_eq!(entries(&file), before);
        sharing.restore().unwrap();
        assert_eq!(entries(&file), before);
    }

    #[test]
    fn restore_preserves_permission_entries_added_by_the_user() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::write(&source, "source").unwrap();
        let file = File::open(&source).unwrap();
        let mut sharing = same_user_sharing();
        let before = entries(&file);
        sharing.tree(root.path(), Access::ReadWrite).unwrap();
        let user_entry = Entry {
            principal: sharing.owner,
            permissions: 1 << 9,
            flags: 0,
        };
        let mut changed = Acl::read(&file).unwrap();
        changed.append(&user_entry).unwrap();
        changed.write(&file).unwrap();
        sharing.restore().unwrap();
        let mut expected = before;
        expected.push(user_entry);
        assert_eq!(entries(&file), expected);
    }

    #[test]
    fn hardlinked_source_nodes_are_shared_once_without_application_gating() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::write(&source, "source").unwrap();
        std::fs::hard_link(&source, root.path().join("alias")).unwrap();
        let file = File::open(&source).unwrap();
        let before = entries(&file);
        let mut sharing = same_user_sharing();
        sharing.tree(root.path(), Access::ReadWrite).unwrap();
        assert_eq!(sharing.nodes.len(), 2);
        sharing.restore().unwrap();
        assert_eq!(entries(&file), before);
    }

    #[test]
    fn outgoing_symlink_does_not_grant_its_target() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = outside.path().join("source");
        std::fs::write(&source, "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("external")).unwrap();
        let file = File::open(&source).unwrap();
        let before = entries(&file);
        let mut sharing = same_user_sharing();
        sharing.tree(root.path(), Access::ReadWrite).unwrap();
        assert_eq!(entries(&file), before);
        sharing.restore().unwrap();
    }

    #[test]
    fn explicitly_selected_executable_does_not_share_its_siblings() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("program");
        let sibling = root.path().join("unrelated");
        std::fs::write(&program, "program").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&sibling, "private sibling").unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&program, &alias).unwrap();
        let file = File::open(&program).unwrap();
        let other = File::open(&sibling).unwrap();
        let before = entries(&file);
        let other_before = entries(&other);
        let mut sharing = same_user_sharing();
        sharing.path(&alias, Access::Read).unwrap();
        assert_ne!(entries(&file), before);
        assert_eq!(entries(&other), other_before);
        sharing.restore().unwrap();
        assert_eq!(entries(&file), before);
    }

    #[test]
    fn changed_contents_and_directory_name_do_not_block_permission_cleanup() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let source = root.join("source");
        std::fs::write(&source, "first").unwrap();
        let file = File::open(&source).unwrap();
        let before = entries(&file);
        let mut sharing = same_user_sharing();
        sharing.tree(&root, Access::ReadWrite).unwrap();
        std::fs::write(&source, "second").unwrap();
        std::fs::rename(&root, parent.path().join("renamed")).unwrap();
        sharing.restore().unwrap();
        assert_eq!(entries(&file), before);
    }

    #[test]
    fn source_tree_larger_than_the_initial_descriptor_limit_is_supported() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..800 {
            std::fs::write(root.path().join(format!("source-{index}")), "source").unwrap();
        }
        let mut sharing = same_user_sharing();
        sharing.tree(root.path(), Access::ReadWrite).unwrap();
        assert_eq!(sharing.nodes.len(), 801);
        sharing.restore().unwrap();
    }

    #[test]
    fn native_sharing_restores_preexisting_entries_and_modes() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let source = nested.join("source");
        std::fs::write(&source, "source").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        let file = File::open(&source).unwrap();
        let before = Acl::read(&file)
            .unwrap()
            .entries()
            .unwrap()
            .into_iter()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        let uid = rustix::process::getuid().as_raw();
        let mut principal = [0; 16];
        // SAFETY: membership writes one UUID into the writable test buffer.
        assert_eq!(unsafe { mbr_uid_to_uuid(uid, principal.as_mut_ptr()) }, 0);
        let mut sharing = Sharing {
            worker_uid: u32::MAX,
            worker: principal,
            owner: principal,
            nodes: BTreeMap::new(),
            recorder: None,
        };
        sharing.tree(root.path(), Access::ReadWrite).unwrap();
        sharing.restore().unwrap();
        assert_eq!(
            Acl::read(&file)
                .unwrap()
                .entries()
                .unwrap()
                .into_iter()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
    }
}
