//! Scoped ACL additions for a private worker and normal-owner access to its outputs.

pub mod outputs;

use std::collections::{BTreeMap, BTreeSet};
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
const DELETE: u32 = 1 << 4;
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
    fn acl_create_entry_np(acl: *mut *mut c_void, output: *mut *mut c_void, index: c_int) -> c_int;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Tag {
    #[default]
    Allow,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    #[serde(default)]
    tag: Tag,
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
    /// Whether this record still needs restoration after another recovery path may have run.
    fn is_pending(&self, _identity: u64) -> bool {
        true
    }
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
        || Acl::read(file)?.entries()?.iter().any(|(_, entry)| {
            entry.tag == Tag::Allow && entry.principal != principal && entry.permissions != 0
        })
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
        self.restore_for(worker_uid, false).map(|_| ())
    }

    /// Revoke an allow-only record; return false without changing a record containing denials.
    pub fn revoke_access(&self, worker_uid: u32) -> io::Result<bool> {
        self.restore_for(worker_uid, true)
    }

    fn restore_for(&self, worker_uid: u32, retain_denials: bool) -> io::Result<bool> {
        let sharing = Sharing::new(worker_uid)?;
        if self.additions.is_empty()
            || self.additions.iter().any(|entry| match entry.tag {
                Tag::Allow => {
                    ![sharing.worker, sharing.owner].contains(&entry.principal)
                        || entry.permissions & !(READ | WRITE | EXECUTE | WRITE_SECURITY) != 0
                        || entry.flags & !(FILE_INHERIT | DIRECTORY_INHERIT) != 0
                }
                Tag::Deny => {
                    entry.principal != sharing.worker
                        || entry.permissions != DELETE
                        || entry.flags != 0
                }
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "permission recovery evidence is not owned",
            ));
        }
        if retain_denials && self.additions.iter().any(|entry| entry.tag == Tag::Deny) {
            return Ok(false);
        }
        if let Some(file) = self.resolved_file()? {
            self.restore_file(&file)?;
        }
        Ok(true)
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
                && let Some((entry, _)) = if owned.tag == Tag::Deny {
                    current.first()
                } else {
                    current.last()
                }
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
        self.tag == requested.tag
            && self.principal == requested.principal
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
            if tag == ALLOW || tag == 2 {
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
                        tag: if tag == ALLOW { Tag::Allow } else { Tag::Deny },
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
        self.insert(value, if value.tag == Tag::Deny { 0 } else { -2 })
    }

    fn insert(&mut self, value: &Entry, index: c_int) -> io::Result<()> {
        let mut pointer = self.0.as_ptr();
        let mut entry = std::ptr::null_mut();
        // SAFETY: the API may reallocate this owned ACL and fills a descriptor into its new storage.
        let code = unsafe { acl_create_entry_np(&mut pointer, &mut entry, index) };
        self.0 = NonNull::new(pointer).ok_or_else(io::Error::last_os_error)?;
        check(code)?;
        let mut permissions = std::ptr::null_mut();
        let mut flags = std::ptr::null_mut();
        // SAFETY: entry belongs to the owned ACL; UUID storage stays live during the copying setter.
        unsafe {
            check(acl_set_tag_type(
                entry,
                if value.tag == Tag::Deny { 2 } else { ALLOW },
            ))?;
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

fn children(file: &File, mut visit: impl FnMut(File) -> io::Result<()>) -> io::Result<()> {
    for entry in descriptor_capacity(|| Ok(Dir::read_from(file)?))? {
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
                file,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?)
        }) {
            Ok(file) => File::from(file),
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
                continue;
            }
            Err(error) => return Err(error),
        };
        visit(opened)?;
    }
    Ok(())
}

#[derive(Default)]
struct Exclusions {
    roots: BTreeSet<(u64, u64)>,
    ancestors: BTreeSet<(u64, u64)>,
    guards: BTreeSet<(u64, u64)>,
    // Pin protected identities until the traversal has finished, including hard-link aliases.
    files: Vec<File>,
    bindings: Vec<(std::path::PathBuf, FileIdentity)>,
}

impl Exclusions {
    fn new(paths: &[std::path::PathBuf], selected: &Path) -> io::Result<Self> {
        let mut result = Self::default();
        for path in paths {
            let canonical = path.canonicalize()?;
            for (index, path) in canonical.ancestors().enumerate() {
                let file = File::from(descriptor_capacity(|| {
                    Ok(open(
                        path,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?)
                })?);
                let metadata = file.metadata()?;
                let identity = (metadata.dev(), metadata.ino());
                if path.starts_with(selected)
                    || (index == 0 && metadata.is_file() && metadata.nlink() > 1)
                {
                    result.guards.insert(identity);
                }
                if index == 0 {
                    result.roots.insert(identity);
                    result
                        .bindings
                        .push((canonical.clone(), FileIdentity::read(&file)?));
                    if metadata.is_dir() {
                        result.hard_links(&file)?;
                    }
                } else {
                    result.ancestors.insert(identity);
                }
                result.files.push(file);
            }
        }
        Ok(result)
    }

    fn verify(&self) -> io::Result<()> {
        for (path, identity) in &self.bindings {
            let file = File::from(open(
                path,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            if FileIdentity::read(&file)? != *identity {
                return Err(io::Error::other(
                    "protected path changed during workspace preparation",
                ));
            }
        }
        Ok(())
    }

    fn hard_links(&mut self, directory: &File) -> io::Result<()> {
        for entry in descriptor_capacity(|| Ok(Dir::read_from(directory)?))? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let metadata =
                match rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(metadata) => metadata,
                    Err(rustix::io::Errno::NOENT) => continue,
                    Err(error) => return Err(error.into()),
                };
            let kind = FileType::from_raw_mode(metadata.st_mode);
            if kind != FileType::Directory
                && !(kind == FileType::RegularFile && metadata.st_nlink > 1)
            {
                continue;
            }
            let file = File::from(descriptor_capacity(|| {
                Ok(openat(
                    directory,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )?)
            })?);
            let pinned = file.metadata()?;
            if pinned.dev() != metadata.st_dev as u64 || pinned.ino() != metadata.st_ino {
                return Err(io::Error::other(
                    "protected evidence changed during hard-link inspection",
                ));
            }
            if kind == FileType::Directory {
                self.hard_links(&file)?;
            } else {
                self.roots.insert((pinned.dev(), pinned.ino()));
                self.guards.insert((pinned.dev(), pinned.ino()));
                self.files.push(file);
            }
        }
        Ok(())
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
        self.grant(file, Access::Search, false, &Exclusions::default())
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
        self.grant(file, access, true, &Exclusions::default())
    }

    /// Share an explicitly selected file or directory, resolving the selected root's symlink.
    /// Descendant symlinks are still skipped; search-only access requires a directory.
    pub fn path(&mut self, path: &Path, access: Access) -> io::Result<()> {
        self.path_excluding(path, access, &[])
    }

    /// Share a selected path without granting access to protected trees or their hard-link aliases.
    /// Journaled delete denials protect the evidence roots and their ancestors during the lease.
    pub fn path_excluding(
        &mut self,
        path: &Path,
        access: Access,
        protected: &[std::path::PathBuf],
    ) -> io::Result<()> {
        let canonical = path.canonicalize()?;
        for protected in protected {
            if canonical.starts_with(protected.canonicalize()?) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "selected path is protected Glass evidence or service state",
                ));
            }
        }
        let exclusions = Exclusions::new(protected, &canonical)?;
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
        let identity = (metadata.dev(), metadata.ino());
        if exclusions.roots.contains(&identity) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "selected path is protected Glass evidence or service state",
            ));
        }
        let mut guarded = BTreeSet::new();
        for protected in &exclusions.files {
            let metadata = protected.metadata()?;
            let identity = (metadata.dev(), metadata.ino());
            if exclusions.guards.contains(&identity) && guarded.insert(identity) {
                if metadata.uid() == self.worker_uid {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "protected evidence and its workspace ancestors must be controlled by the normal owner",
                    ));
                }
                self.add_entries(
                    protected,
                    vec![Entry {
                        tag: Tag::Deny,
                        principal: self.worker,
                        permissions: DELETE,
                        flags: 0,
                    }],
                )?;
            }
        }
        exclusions.verify()?;
        self.grant(file, access, access != Access::Search, &exclusions)?;
        exclusions.verify()
    }

    fn grant(
        &mut self,
        file: File,
        access: Access,
        recursive: bool,
        exclusions: &Exclusions,
    ) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Ok(());
        }
        let identity = (metadata.dev(), metadata.ino());
        let protected = exclusions.roots.contains(&identity);
        let directory = metadata.is_dir();
        let ancestor = directory && exclusions.ancestors.contains(&identity);
        let mut rights = if access == Access::Search { 0 } else { READ };
        if access == Access::ReadWrite {
            rights |= if ancestor { WRITE & !DELETE } else { WRITE };
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
        if !protected && !worker_already_owns_access {
            entries.push(Entry {
                tag: Tag::Allow,
                principal: self.worker,
                permissions: rights,
                flags: inherited,
            });
        }
        if !protected && directory && access == Access::ReadWrite {
            entries.push(Entry {
                tag: Tag::Allow,
                principal: self.owner,
                permissions: READ | WRITE | EXECUTE | WRITE_SECURITY,
                flags: inherited,
            });
        }
        self.add_entries(&file, entries)?;
        if recursive && directory && !protected {
            children(&file, |child| self.grant(child, access, true, exclusions))?;
        }
        Ok(())
    }

    fn add_entries(&mut self, file: &File, entries: Vec<Entry>) -> io::Result<()> {
        let metadata = file.metadata()?;
        let identity = (metadata.dev(), metadata.ino());
        let mut acl = Acl::read(file)?;
        let existing = acl.entries()?;
        let additions: Vec<Entry> = entries
            .into_iter()
            .filter(|entry| {
                if entry.tag == Tag::Deny {
                    !(self
                        .nodes
                        .get(&identity)
                        .is_some_and(|node| node.owned.contains(entry))
                        && existing
                            .first()
                            .is_some_and(|(_, current)| current == entry))
                } else {
                    !existing.iter().any(|(_, current)| current.covers(entry))
                }
            })
            .collect();
        if !additions.is_empty() {
            let change = PermissionChange {
                identity: FileIdentity::read(file)?,
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
            acl.write(file)?;
        }
        Ok(())
    }

    /// Remove only this connection's additions from the current ACLs, retaining user edits.
    /// Errors keep unresolved entries owned by this object for an explicit retry or inspection.
    pub fn restore(&mut self) -> io::Result<()> {
        // Keep evidence denials until every temporary allow has been removed.
        for tag in [Tag::Allow, Tag::Deny] {
            let mut failure = None;
            for node in self.nodes.values_mut() {
                if !node.owned.iter().any(|entry| entry.tag == tag) {
                    continue;
                }
                let result = (|| {
                    if let Some(recorder) = &self.recorder {
                        for index in (0..node.records.len()).rev() {
                            let (record, change) = &node.records[index];
                            if !change.additions.iter().any(|entry| entry.tag == tag) {
                                continue;
                            }
                            let mut recorder = recorder
                                .lock()
                                .map_err(|_| io::Error::other("permission recorder is poisoned"))?;
                            if recorder.is_pending(*record) {
                                change.restore_file(&node.file)?;
                                recorder.settled(*record)?;
                            }
                            node.records.remove(index);
                        }
                    } else {
                        let acl = Acl::read(&node.file)?;
                        for owned in node.owned.iter().filter(|entry| entry.tag == tag) {
                            let entries = acl.entries()?;
                            let entry = if tag == Tag::Deny {
                                entries.into_iter().find(|(_, entry)| entry == owned)
                            } else {
                                entries.into_iter().rev().find(|(_, entry)| entry == owned)
                            };
                            if let Some((entry, _)) = entry {
                                // SAFETY: entry belongs to this live ACL and is discarded after deletion.
                                check(unsafe { acl_delete_entry(acl.0.as_ptr(), entry) })?;
                            }
                        }
                        acl.write(&node.file)?;
                    }
                    node.owned.retain(|entry| entry.tag != tag);
                    Ok::<_, io::Error>(())
                })();
                if let Err(error) = result {
                    failure = Some(error);
                }
            }
            self.nodes.retain(|_, node| !node.owned.is_empty());
            if let Some(error) = failure {
                return Err(error);
            }
        }
        Ok(())
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
        protected: Option<File>,
    }

    impl RecordChanges for Recorded {
        fn is_pending(&self, id: u64) -> bool {
            !self.settled.contains(&id)
        }
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
            if let Some(protected) = &self.protected
                && change.additions.iter().any(|entry| entry.tag == Tag::Allow)
            {
                assert!(
                    entries(protected)
                        .iter()
                        .any(|entry| entry.tag == Tag::Deny && entry.permissions == DELETE),
                    "evidence denial must precede every allow"
                );
            }
            self.changes.push(change.clone());
            Ok(self.changes.len() as u64)
        }
        fn settled(&mut self, id: u64) -> io::Result<()> {
            if self.protected.is_some()
                && self.changes[id as usize - 1]
                    .additions
                    .iter()
                    .any(|entry| entry.tag == Tag::Deny)
            {
                for change in &self.changes {
                    if let Some(file) = change.resolved_file()? {
                        let current = entries(&file);
                        for entry in change
                            .additions
                            .iter()
                            .filter(|entry| entry.tag == Tag::Allow)
                        {
                            assert!(
                                current.iter().filter(|value| *value == entry).count()
                                    <= change.before.iter().filter(|value| *value == entry).count(),
                                "all temporary allows must be removed before any denial"
                            );
                        }
                    }
                }
            }
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
            tag: Tag::Allow,
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
    fn evidence_protection_precedes_allows_and_outlives_them_during_restore() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join("nested/.traces");
        std::fs::create_dir_all(&evidence).unwrap();
        std::fs::write(root.path().join("source"), "source").unwrap();
        let recorder = Arc::new(Mutex::new(Recorded {
            protected: Some(File::open(&evidence).unwrap()),
            ..Default::default()
        }));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder.clone());
        sharing
            .path_excluding(root.path(), Access::ReadWrite, &[evidence])
            .unwrap();
        let changes = recorder.lock().unwrap().changes.clone();
        let first_allow = changes
            .iter()
            .position(|change| change.additions.iter().any(|entry| entry.tag == Tag::Allow))
            .unwrap();
        assert!(
            changes[..first_allow]
                .iter()
                .all(|change| change.additions.iter().all(|entry| entry.tag == Tag::Deny))
        );
        assert!(
            changes[first_allow..]
                .iter()
                .all(|change| change.additions.iter().all(|entry| entry.tag == Tag::Allow))
        );
        sharing.restore().unwrap();
        assert_eq!(recorder.lock().unwrap().settled.len(), changes.len());
    }

    #[test]
    fn overlapping_project_shares_keep_one_guard_until_reverse_recovery_removes_the_allows() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join(".traces");
        std::fs::create_dir(&evidence).unwrap();
        let evidence_file = File::open(&evidence).unwrap();
        let recorder = Arc::new(Mutex::new(Recorded::default()));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder.clone());
        sharing
            .path_excluding(
                root.path(),
                Access::ReadWrite,
                std::slice::from_ref(&evidence),
            )
            .unwrap();
        let initial = recorder.lock().unwrap().changes.len();
        sharing
            .path_excluding(&root.path().join("."), Access::Read, &[evidence])
            .unwrap();
        assert_eq!(recorder.lock().unwrap().changes.len(), initial);
        let changes = recorder.lock().unwrap().changes.clone();
        for change in changes.iter().rev() {
            if change.additions.iter().any(|entry| entry.tag == Tag::Allow) {
                assert!(
                    entries(&evidence_file)
                        .iter()
                        .any(|entry| entry.tag == Tag::Deny)
                );
            }
            change
                .restore_file(&change.resolved_file().unwrap().unwrap())
                .unwrap();
        }
        sharing.restore().unwrap();
    }

    #[test]
    fn owner_created_and_atomically_replaced_files_remain_shared_while_evidence_does_not_inherit() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join(".traces");
        std::fs::create_dir(&evidence).unwrap();
        let source = root.path().join("source");
        std::fs::write(&source, "old").unwrap();
        let mut sharing = same_user_sharing();
        sharing.owner = [1; 16];
        sharing
            .path_excluding(
                root.path(),
                Access::ReadWrite,
                std::slice::from_ref(&evidence),
            )
            .unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, "new").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&replacement, &source).unwrap();
        let current = File::open(&source).unwrap();
        assert!(entries(&current).iter().any(|entry| entry.tag == Tag::Allow
            && entry.principal == sharing.worker
            && entry.permissions & READ != 0));
        let secret = evidence.join("new-evidence");
        std::fs::write(&secret, "private").unwrap();
        assert!(
            !entries(&File::open(secret).unwrap())
                .iter()
                .any(|entry| entry.tag == Tag::Allow && entry.principal == sharing.worker)
        );
        sharing.restore().unwrap();
    }

    #[test]
    fn recorded_evidence_denials_recover_from_serialized_changes() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join(".traces");
        std::fs::create_dir(&evidence).unwrap();
        let file = File::open(&evidence).unwrap();
        let before = entries(&file);
        let recorder = Arc::new(Mutex::new(Recorded::default()));
        let mut sharing = same_user_sharing();
        sharing.recorder = Some(recorder.clone());
        sharing
            .path_excluding(root.path(), Access::ReadWrite, &[evidence])
            .unwrap();
        let changes = recorder.lock().unwrap().changes.clone();
        assert!(
            changes
                .iter()
                .any(|change| change.additions.iter().any(|entry| entry.tag == Tag::Deny))
        );
        for change in changes.iter().rev() {
            let copy: PermissionChange =
                serde_json::from_slice(&serde_json::to_vec(change).unwrap()).unwrap();
            let resolved = copy.resolved_file().unwrap().unwrap();
            copy.restore_file(&resolved).unwrap();
            copy.restore_file(&resolved).unwrap();
        }
        sharing.restore().unwrap();
        assert_eq!(entries(&file), before);
        assert_eq!(recorder.lock().unwrap().settled.len(), changes.len());
    }

    #[test]
    fn mixed_records_wait_for_full_restore_without_partial_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let file = File::create(directory.path().join("workspace")).unwrap();
        let sharing = Sharing::new(u32::MAX - 1).unwrap(); // Darwin's nobody account.
        let before = entries(&file);
        let allow = Entry {
            tag: Tag::Allow,
            principal: sharing.worker,
            permissions: READ,
            flags: 0,
        };
        let denial = Entry {
            tag: Tag::Deny,
            principal: sharing.worker,
            permissions: DELETE,
            flags: 0,
        };
        let change = PermissionChange {
            identity: FileIdentity::read(&file).unwrap(),
            before: before.clone(),
            additions: vec![allow.clone(), denial.clone()],
        };
        let user_edit = Entry {
            tag: Tag::Allow,
            principal: sharing.owner,
            permissions: EXECUTE,
            flags: 0,
        };
        let mut acl = Acl::read(&file).unwrap();
        for entry in [&allow, &denial, &user_edit] {
            acl.append(entry).unwrap();
        }
        acl.write(&file).unwrap();

        assert!(!change.revoke_access(sharing.worker_uid).unwrap());
        assert!(entries(&file).contains(&allow));
        assert!(entries(&file).contains(&denial));
        assert!(entries(&file).contains(&user_edit));
        let revoked = entries(&file);
        assert!(!change.revoke_access(sharing.worker_uid).unwrap());
        assert_eq!(entries(&file), revoked);

        change.restore(sharing.worker_uid).unwrap();
        let mut expected = before;
        expected.push(user_edit);
        assert_eq!(entries(&file), expected);
    }

    #[test]
    fn an_allow_only_record_is_fully_settled_by_access_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let file = File::create(directory.path().join("workspace")).unwrap();
        let sharing = Sharing::new(u32::MAX - 1).unwrap();
        let before = entries(&file);
        let allow = Entry {
            tag: Tag::Allow,
            principal: sharing.worker,
            permissions: READ,
            flags: 0,
        };
        let change = PermissionChange {
            identity: FileIdentity::read(&file).unwrap(),
            before: before.clone(),
            additions: vec![allow.clone()],
        };
        let mut acl = Acl::read(&file).unwrap();
        acl.append(&allow).unwrap();
        acl.write(&file).unwrap();

        assert!(change.revoke_access(sharing.worker_uid).unwrap());
        assert_eq!(entries(&file), before);
        assert!(change.revoke_access(sharing.worker_uid).unwrap());
    }

    #[test]
    fn a_retained_scope_does_not_restore_an_already_settled_record_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source");
        let file = File::create(&path).unwrap();
        let before = entries(&file);
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut sharing = Sharing::recorded(u32::MAX - 1, recorded.clone()).unwrap();
        sharing.path(&path, Access::Read).unwrap();
        let change = recorded.lock().unwrap().changes[0].clone();
        assert!(change.revoke_access(u32::MAX - 1).unwrap());
        recorded.lock().unwrap().settled(1).unwrap();

        let user_edit = change.additions[0].clone();
        let mut acl = Acl::read(&file).unwrap();
        acl.append(&user_edit).unwrap();
        acl.write(&file).unwrap();
        sharing.restore().unwrap();
        let mut expected = before;
        expected.push(user_edit);
        assert_eq!(entries(&file), expected);
        assert_eq!(recorded.lock().unwrap().settled, vec![1]);
    }

    #[test]
    fn legacy_journal_entries_default_to_allow_without_losing_their_permissions() {
        let entry: Entry = serde_json::from_value(serde_json::json!({
            "principal": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1], "permissions": READ, "flags": FILE_INHERIT
        })).unwrap();
        assert_eq!(entry.tag, Tag::Allow);
        assert_eq!(entry.permissions, READ);
        assert_eq!(entry.flags, FILE_INHERIT);
    }

    #[test]
    fn a_replaced_protected_path_is_not_accepted_as_the_pinned_evidence_root() {
        let root = tempfile::tempdir().unwrap();
        let protected = root.path().join("evidence");
        std::fs::create_dir(&protected).unwrap();
        let exclusions = Exclusions::new(std::slice::from_ref(&protected), root.path()).unwrap();
        std::fs::rename(&protected, root.path().join("original-evidence")).unwrap();
        std::fs::create_dir(&protected).unwrap();
        assert!(exclusions.verify().is_err());
    }

    #[test]
    fn excluded_evidence_and_its_ancestors_remain_in_place_while_outputs_are_writable() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let evidence = project.join("nested/.traces");
        std::fs::create_dir_all(&evidence).unwrap();
        std::fs::write(evidence.join("secret"), "evidence").unwrap();
        let source = project.join("source");
        std::fs::write(&source, "source").unwrap();
        let project_file = File::open(&project).unwrap();
        let evidence_file = File::open(&evidence).unwrap();
        let secret_file = File::open(evidence.join("secret")).unwrap();
        let original = entries(&evidence_file);
        let secret_original = entries(&secret_file);
        let mut sharing = same_user_sharing();
        // A distinct output-owner principal lets this test exercise worker ACLs as an ordinary user.
        sharing.owner = [1; 16];
        sharing
            .path_excluding(&project, Access::ReadWrite, std::slice::from_ref(&evidence))
            .unwrap();
        assert_eq!(entries(&secret_file), secret_original);
        assert_eq!(entries(&evidence_file)[0].tag, Tag::Deny);
        assert_eq!(entries(&project_file)[0].permissions, DELETE);
        assert!(std::fs::rename(&evidence, project.join("moved-evidence")).is_err());
        assert!(std::fs::rename(&project, root.path().join("moved-project")).is_err());
        assert!(std::fs::rename(project.join("nested"), project.join("moved-parent")).is_err());
        std::fs::write(&source, "updated source").unwrap();
        std::fs::remove_file(&source).unwrap();
        let output = project.join("output");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("new"), "build output").unwrap();
        let output_file = File::open(&output).unwrap();
        assert!(
            entries(&output_file)
                .iter()
                .any(|entry| entry.tag == Tag::Allow
                    && entry.principal == sharing.worker
                    && entry.permissions & READ != 0)
        );
        assert!(entries(&output_file).iter().any(
            |entry| entry.principal == sharing.owner && entry.permissions & WRITE_SECURITY != 0
        ));
        std::fs::remove_dir_all(&output).unwrap();
        sharing.restore().unwrap();
        assert_eq!(entries(&evidence_file), original);
        std::fs::rename(&project, root.path().join("moved-project")).unwrap();
    }

    #[test]
    fn protected_descendant_hardlinks_are_not_granted_through_workspace_aliases() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join(".traces");
        std::fs::create_dir(&evidence).unwrap();
        let secret = evidence.join("secret");
        std::fs::write(&secret, "evidence").unwrap();
        let alias = root.path().join("innocent-alias");
        std::fs::hard_link(&secret, &alias).unwrap();
        let secret_file = File::open(&secret).unwrap();
        let before = entries(&secret_file);
        let mut sharing = same_user_sharing();
        sharing.owner = [1; 16];
        sharing
            .path_excluding(
                root.path(),
                Access::ReadWrite,
                std::slice::from_ref(&evidence),
            )
            .unwrap();
        let after = entries(&secret_file);
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(
            after[0],
            Entry {
                tag: Tag::Deny,
                principal: sharing.worker,
                permissions: DELETE,
                flags: 0
            }
        );
        assert!(std::fs::remove_file(&alias).is_err());
        sharing.restore().unwrap();
        assert_eq!(entries(&secret_file), before);
    }

    #[test]
    fn selecting_an_excluded_path_or_alias_refuses_before_any_mutation() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join(".traces");
        std::fs::create_dir(&evidence).unwrap();
        let secret = evidence.join("secret");
        std::fs::write(&secret, "evidence").unwrap();
        let alias = root.path().join("alias");
        std::fs::hard_link(&secret, &alias).unwrap();
        for (selected, protected) in [
            (&evidence, &evidence),
            (&secret, &evidence),
            (&alias, &secret),
        ] {
            let mut sharing = same_user_sharing();
            assert!(
                sharing
                    .path_excluding(selected, Access::ReadWrite, std::slice::from_ref(protected))
                    .is_err()
            );
            assert!(sharing.nodes.is_empty());
        }
    }

    #[test]
    fn evidence_delete_denial_precedes_existing_allow_and_restores_its_original_order() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join("evidence");
        std::fs::create_dir(&evidence).unwrap();
        let file = File::open(&evidence).unwrap();
        let mut sharing = same_user_sharing();
        let allow = Entry {
            tag: Tag::Allow,
            principal: sharing.worker,
            permissions: DELETE,
            flags: 0,
        };
        let deny = Entry {
            tag: Tag::Deny,
            ..allow.clone()
        };
        let mut acl = Acl::read(&file).unwrap();
        acl.append(&allow).unwrap();
        acl.insert(&deny, -2).unwrap();
        acl.write(&file).unwrap();
        let before = entries(&file);
        sharing
            .path_excluding(root.path(), Access::ReadWrite, &[evidence])
            .unwrap();
        assert_eq!(entries(&file)[0], deny);
        sharing.restore().unwrap();
        assert_eq!(entries(&file), before);
    }

    #[test]
    fn inherited_permissions_cover_access_without_rewriting_their_acl() {
        let requested = Entry {
            tag: Tag::Allow,
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
            tag: Tag::Allow,
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
