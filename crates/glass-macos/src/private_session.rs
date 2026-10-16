//! Protected account enrollment and live attachment to its private Aqua login.

use glass_core::{GlassError, Result};

unsafe extern "C" {
    fn glass_private_is_owner() -> i32;
    fn glass_private_attach(onboarding: i32) -> i32;
    fn glass_private_still_private() -> i32;
    fn glass_private_owner_uid() -> u32;
    fn glass_private_worker_uid() -> u32;
    fn glass_private_owner_home() -> *mut std::ffi::c_char;
    fn free(pointer: *mut std::ffi::c_void);
}

pub fn attach(onboarding: bool) -> Result<()> {
    // SAFETY: the linked reader takes one integer and mutates only its own attachment cache.
    let code = unsafe { glass_private_attach(i32::from(onboarding)) };
    check(code)
}

pub fn require_private() -> Result<()> {
    // SAFETY: the linked reader has no arguments and only reads session state and its cache.
    check(unsafe { glass_private_still_private() })
}

fn check(code: i32) -> Result<()> {
    match code {
        0 => Ok(()),
        77 => Err(GlassError::Backend(
            "macOS worker requires its enrolled, non-console Aqua login".into(),
        )),
        _ => Err(GlassError::Backend(
            "macOS worker account enrollment is unavailable or does not match".into(),
        )),
    }
}

pub fn owner_uid() -> u32 {
    // SAFETY: the linked getter reads one atomic account-owner identifier and takes no pointers.
    unsafe { glass_private_owner_uid() }
}

pub fn is_owner() -> bool {
    // SAFETY: this reader takes no arguments and does not mutate the account or GUI.
    unsafe { glass_private_is_owner() != 0 }
}

pub fn worker_uid() -> u32 {
    // SAFETY: this getter reads protected enrollment for its ordinary owner and takes no pointers.
    unsafe { glass_private_worker_uid() }
}

pub fn owner_home() -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: the getter returns a caller-owned NUL-terminated copy, or null.
    let pointer = unsafe { glass_private_owner_home() };
    if pointer.is_null() {
        return None;
    }
    // SAFETY: pointer is live and NUL-terminated until its allocation is freed below.
    let value = unsafe { std::ffi::CStr::from_ptr(pointer) }.to_bytes();
    let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(value));
    // SAFETY: the getter allocated this copy with strdup; no borrower remains.
    unsafe { free(pointer.cast()) };
    Some(path)
}

unsafe extern "C" {
    fn glass_private_owns_pid(pid: i32) -> i32;
}
/// Whether the live process still belongs to this pinned private account.
pub fn owns_pid(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: the integer-only helper reads kernel metadata and posts no input.
    unsafe { glass_private_owns_pid(pid) == 1 }
}

unsafe extern "C" {
    fn glass_private_attached() -> i32;
}
/// Whether this process pinned a private worker attachment at entry.
pub fn attached() -> bool {
    // SAFETY: the getter reads process-local atomics and has no effects.
    unsafe { glass_private_attached() == 1 }
}

/// Kernel process identities in the selected account; no names or GUI APIs are queried.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(C)]
pub struct ProcessIdentity {
    pid: u32,
    seconds: u64,
    microseconds: u64,
}

/// Read a bounded account process set, retaining birth time against PID reuse.
pub fn account_processes(uid: u32) -> std::io::Result<Vec<ProcessIdentity>> {
    unsafe extern "C" {
        fn glass_private_processes(uid: u32, out: *mut ProcessIdentity, capacity: i32) -> i32;
    }
    let mut processes = vec![ProcessIdentity::default(); 8192];
    // SAFETY: the helper writes at most the specified number of repr(C) entries.
    let count = unsafe { glass_private_processes(uid, processes.as_mut_ptr(), 8192) };
    if count < 0 {
        return Err(std::io::Error::from_raw_os_error(-count));
    }
    processes.truncate(count as usize);
    Ok(processes)
}

/// Installed enrollment directory; this is service state, never application eligibility.
pub const ROOT: &str = "/Library/Application Support/Glass";

/// Root-owned registration for one normal account; another user's registration has no effect.
pub fn root_for(owner: u32) -> std::path::PathBuf {
    std::path::Path::new(ROOT)
        .join("accounts")
        .join(owner.to_string())
}

/// Resolve explicit enrollment without silently returning to the shared desktop.
pub fn configured() -> Result<bool> {
    let owner = rustix::process::geteuid().as_raw();
    let root = root_for(owner);
    if std::fs::symlink_metadata(&root)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        && std::fs::symlink_metadata(std::path::Path::new(ROOT).join("account.plist")).is_ok()
        && legacy_owner()? == owner
    {
        return Err(GlassError::Backend("legacy macOS enrollment requires migration; run glass-mcp setup --account ACCOUNT or glass-mcp unenroll".into()));
    }
    configured_at(&root, is_owner)
}

fn configured_at(root: &std::path::Path, verify: impl FnOnce() -> bool) -> Result<bool> {
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
        Ok(_) if verify() => Ok(true),
        Ok(_) => Err(GlassError::Backend(
            "macOS account enrollment is incomplete or invalid; run glass-mcp setup --account ACCOUNT or glass-mcp unenroll".into(),
        )),
    }
}

/// Owner of an older preview registration, read without requiring its live login.
pub fn legacy_owner() -> Result<u32> {
    unsafe extern "C" {
        fn glass_private_legacy_owner() -> u32;
    }
    // SAFETY: this reader validates root-owned metadata and returns no pointers.
    let owner = unsafe { glass_private_legacy_owner() };
    if owner < 501 {
        return Err(GlassError::Backend(
            "legacy macOS enrollment owner cannot be verified".into(),
        ));
    }
    Ok(owner)
}

/// Bind a worker to the normal owner's registration before reading session metadata.
pub fn attach_for(owner: u32, onboarding: bool) -> Result<()> {
    select_owner(owner)?;
    attach(onboarding)
}

fn select_owner(owner: u32) -> Result<()> {
    unsafe extern "C" {
        fn glass_private_set_owner(owner: u32) -> i32;
    }
    // SAFETY: this setter accepts only an ordinary numeric UID and refuses a changed attachment.
    check(unsafe { glass_private_set_owner(owner) })
}

/// Waitable onboarding attachment; an unfinished first login is pending, not a failed grant.
pub fn try_attach_for_onboarding(owner: u32) -> Result<bool> {
    select_owner(owner)?;
    if !is_worker() {
        return Err(GlassError::Backend(
            "permission onboarding has no matching account enrollment".into(),
        ));
    }
    // SAFETY: the helper reads this process's session and pins only a valid attachment.
    let code = unsafe { glass_private_attach(1) };
    match code {
        0 => Ok(true),
        77 => Ok(false),
        _ => {
            check(code)?;
            Ok(false)
        }
    }
}

pub fn require_if_attached() -> Result<()> {
    if attached() {
        require_private()
    } else {
        Ok(())
    }
}

pub fn is_worker() -> bool {
    unsafe extern "C" {
        fn glass_private_is_worker() -> i32;
    }
    // SAFETY: the reader validates protected enrollment and this process's account only.
    unsafe { glass_private_is_worker() == 1 }
}

/// ACL-aware write protection for retained service-state descriptors.
pub fn require_write_protected(file: &std::fs::File, owner: u32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn glass_private_writes_protected(fd: i32, owner: u32) -> i32;
    }
    // SAFETY: the native helper borrows this live descriptor and reads its ACL only.
    let code = unsafe { glass_private_writes_protected(file.as_raw_fd(), owner) };
    if code == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(code))
    }
}

pub fn session_id() -> Result<u32> {
    unsafe extern "C" {
        fn glass_private_session_id() -> u32;
    }
    // SAFETY: this getter reads only the process-local pinned session identity.
    let id = unsafe { glass_private_session_id() };
    if id == 0 || id == u32::MAX {
        Err(GlassError::Backend("private session is not pinned".into()))
    } else {
        Ok(id)
    }
}

/// An immutable per-boot identity, independent of calendar-clock adjustments.
pub fn boot_identity() -> std::io::Result<[u8; 16]> {
    unsafe extern "C" {
        fn glass_private_boot_identity(out: *mut u8) -> i32;
    }
    let mut bytes = [0; 16];
    // SAFETY: the helper writes one UUID to this live 16-byte buffer.
    let code = unsafe { glass_private_boot_identity(bytes.as_mut_ptr()) };
    if code == 0 && bytes != [0; 16] {
        Ok(bytes)
    } else {
        Err(std::io::Error::from_raw_os_error(if code == 0 {
            5
        } else {
            code
        }))
    }
}

/// Query only an audit-session lifetime; neither log out nor signal any process.
pub fn session_exists(session: u32, worker: u32) -> std::io::Result<bool> {
    unsafe extern "C" {
        fn glass_private_session_exists(session: u32, uid: u32) -> i32;
    }
    // SAFETY: both identifiers are passed by value to a read-only kernel query.
    let code = unsafe { glass_private_session_exists(session, worker) };
    match code {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(std::io::Error::from_raw_os_error(-code)),
    }
}

/// Read-only login hints; these never establish worker identity, grants or readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginObservation {
    NotObserved,
    Incomplete,
    Complete,
}

/// Inspect the dynamic store, requiring a complete owner login as a positive control.
pub fn login_observation(worker: u32) -> std::io::Result<LoginObservation> {
    unsafe extern "C" {
        fn glass_private_login_observation(worker: u32) -> i32;
    }
    // SAFETY: the native helper reads session metadata and returns only a scalar classification.
    match unsafe { glass_private_login_observation(worker) } {
        0 => Ok(LoginObservation::NotObserved),
        1 => Ok(LoginObservation::Incomplete),
        2 => Ok(LoginObservation::Complete),
        code => Err(std::io::Error::from_raw_os_error(-code)),
    }
}

#[test]
fn login_observation_rejects_incomplete_or_ambiguous_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("session-readonly-tests");
    let mut compiler = std::process::Command::new("/usr/bin/clang");
    compiler
        .args(["-fobjc-arc", "-std=c11", "-DGLASS_PRIVATE_SESSION_TEST"])
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/private_session.m"))
        .args(["-lbsm", "-o"])
        .arg(&executable);
    for framework in [
        "Foundation",
        "OpenDirectory",
        "ApplicationServices",
        "Security",
        "SystemConfiguration",
    ] {
        compiler.args(["-framework", framework]);
    }
    let built = compiler.output().unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let checked = std::process::Command::new(executable).output().unwrap();
    assert!(
        checked.status.success(),
        "native parser check returned {}: {}",
        checked.status,
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(String::from_utf8_lossy(&checked.stdout).contains("\"login_observation_cases\":8"));
}

#[test]
#[ignore = "requires a normal GUI owner with an enrolled, logged-in development account; read-only"]
fn login_observation_includes_a_background_development_login() {
    assert!(is_owner());
    assert_eq!(
        login_observation(rustix::process::getuid().as_raw()).unwrap(),
        LoginObservation::Complete
    );
    assert_eq!(
        login_observation(worker_uid()).unwrap(),
        LoginObservation::Complete
    );
}

pub fn current_process() -> std::io::Result<ProcessIdentity> {
    account_processes(rustix::process::getuid().as_raw())?
        .into_iter()
        .find(|process| process.pid == std::process::id())
        .ok_or_else(|| std::io::Error::other("worker process birth identity unavailable"))
}

/// Public signing requirement of this installed service, never a signing credential.
pub fn signing_requirement() -> Result<Vec<u8>> {
    unsafe extern "C" {
        fn glass_private_copy_requirement(out: *mut *mut u8, length: *mut usize) -> i32;
    }
    let mut pointer = std::ptr::null_mut();
    let mut length = 0;
    // SAFETY: the helper initializes both outputs and transfers one malloc-owned byte buffer.
    let code = unsafe { glass_private_copy_requirement(&mut pointer, &mut length) };
    if code != 0 {
        return Err(GlassError::Backend(format!(
            "reading worker signing requirement: {code}"
        )));
    }
    if pointer.is_null() || length == 0 || length > 65536 {
        if !pointer.is_null() {
            /* SAFETY: the helper transferred this malloc allocation. */
            unsafe { free(pointer.cast()) };
        }
        return Err(GlassError::Backend(
            "invalid worker signing requirement".into(),
        ));
    }
    // SAFETY: the successful helper supplies length initialized bytes in this live buffer.
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length) }.to_vec();
    // SAFETY: no borrower remains and the helper allocated this buffer with malloc.
    unsafe { free(pointer.cast()) };
    Ok(bytes)
}

pub fn authenticate_peer(
    stream: &std::os::unix::net::UnixStream,
    requirement: &[u8],
    worker: u32,
) -> Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn glass_private_authenticate_peer(
            fd: i32,
            bytes: *const u8,
            length: usize,
            worker: u32,
            pid: *mut u32,
            session: *mut u32,
        ) -> i32;
    }
    let mut pid = 0;
    let mut session = 0;
    // SAFETY: the socket and requirement stay live; the helper writes two scalar outputs.
    let code = unsafe {
        glass_private_authenticate_peer(
            stream.as_raw_fd(),
            requirement.as_ptr(),
            requirement.len(),
            worker,
            &mut pid,
            &mut session,
        )
    };
    if code == 0 {
        Ok((pid, session))
    } else {
        Err(GlassError::Backend(format!(
            "private worker code authentication failed: {code}"
        )))
    }
}

impl ProcessIdentity {
    pub fn matches_pid(&self, pid: u32) -> bool {
        self.pid == pid
    }
}

/// Authenticate the transient privileged setup role without relaxing worker authentication.
pub fn authenticate_setup_root(
    stream: &std::os::unix::net::UnixStream,
    requirement: &[u8],
    owner: u32,
) -> Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn glass_setup_authenticate_root(
            fd: i32,
            bytes: *const u8,
            length: usize,
            owner: u32,
            pid: *mut u32,
            session: *mut u32,
        ) -> i32;
    }
    let mut pid = 0;
    let mut session = 0;
    // SAFETY: the socket/requirement stay live and the helper initializes both scalar outputs.
    let code = unsafe {
        glass_setup_authenticate_root(
            stream.as_raw_fd(),
            requirement.as_ptr(),
            requirement.len(),
            owner,
            &mut pid,
            &mut session,
        )
    };
    if code == 0 {
        Ok(())
    } else {
        Err(GlassError::Backend(format!(
            "privileged Glass setup authentication failed: {code}"
        )))
    }
}

#[cfg(test)]
mod protection_tests {
    use super::require_write_protected;
    use std::fs::File;
    use std::process::Command;

    struct Directory {
        temporary: tempfile::TempDir,
        file: File,
    }

    impl Directory {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let file = File::open(temporary.path()).unwrap();
            Self { temporary, file }
        }

        fn entry(&self, index: u32, value: &str) {
            assert!(
                Command::new("/bin/chmod")
                    .args(["+a#", &index.to_string(), value])
                    .arg(self.temporary.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }

        fn check(&self) -> std::io::Result<()> {
            require_write_protected(&self.file, rustix::process::getuid().as_raw())
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = Command::new("/bin/chmod")
                .arg("-N")
                .arg(self.temporary.path())
                .status();
        }
    }

    #[test]
    fn an_absent_acl_is_protected() {
        let directory = Directory::new();
        directory.check().unwrap();
    }

    #[test]
    fn a_deny_entry_is_not_an_io_error() {
        let directory = Directory::new();
        directory.entry(0, "everyone deny delete");
        directory.check().unwrap();
    }

    #[test]
    fn foreign_read_access_after_a_deny_entry_is_protected() {
        let directory = Directory::new();
        directory.entry(0, "everyone deny delete");
        directory.entry(1, "everyone allow readattr,readsecurity");
        directory.check().unwrap();
    }

    #[test]
    fn an_owner_write_entry_is_protected() {
        let directory = Directory::new();
        let owner = Command::new("/usr/bin/id").arg("-un").output().unwrap();
        assert!(owner.status.success());
        let name = String::from_utf8(owner.stdout).unwrap();
        directory.entry(0, &format!("user:{} allow write", name.trim()));
        directory.check().unwrap();
    }

    #[test]
    fn a_later_foreign_write_entry_is_refused() {
        let directory = Directory::new();
        directory.entry(0, "everyone deny delete");
        directory.entry(1, "everyone allow write");
        assert_eq!(directory.check().unwrap_err().raw_os_error(), Some(1));
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    #[test]
    fn another_users_incomplete_registration_does_not_select_or_validate_our_route() {
        let registry = tempfile::tempdir().unwrap();
        let other = registry.path().join("501");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("account.plist"), "incomplete").unwrap();
        assert!(
            !configured_at(&registry.path().join("502"), || panic!(
                "another user's registration must not be validated"
            ))
            .unwrap()
        );
        assert!(configured_at(&other, || false).is_err());
    }

    #[test]
    fn existing_owner_registration_never_silently_falls_back_when_verification_fails() {
        let registry = tempfile::tempdir().unwrap();
        assert!(configured_at(registry.path(), || false).is_err());
    }

    #[test]
    fn removing_only_our_registration_restores_shared_use_and_leaves_the_other_user_enrolled() {
        let registry = tempfile::tempdir().unwrap();
        let ours = registry.path().join("501");
        let other = registry.path().join("502");
        std::fs::create_dir(&ours).unwrap();
        std::fs::create_dir(&other).unwrap();
        assert!(configured_at(&ours, || true).unwrap());
        std::fs::remove_dir(&ours).unwrap();
        assert!(
            !configured_at(&ours, || panic!(
                "unenrolled owner must use the normal route"
            ))
            .unwrap()
        );
        assert!(configured_at(&other, || true).unwrap());
    }
}
