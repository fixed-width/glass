//! `sandbox_init` FFI. [`apply_cstr`] is meant to run inside `Command::pre_exec` (post-fork,
//! pre-exec), so it does NO allocation: the caller builds the `CString` first and this makes
//! a single syscall, returning an alloc-free `io::Error` on failure. `sandbox_init` accepts
//! an inline SBPL string with flag `0`; it is deprecated-but-shipping (Apple + Chromium).

use std::ffi::{c_char, c_int, CStr};

extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
}

/// Fork-safe variant for `pre_exec`: a single `sandbox_init` syscall with a pre-built
/// `CString`, no allocation, no error-buffer retrieval. Returns an alloc-free `io::Error`
/// (kind `PermissionDenied`) on failure so the parent's `spawn()` fails cleanly.
pub fn apply_cstr(profile: &CStr) -> std::io::Result<()> {
    // SAFETY: `profile` is a valid NUL-terminated C string; a null `errorbuf` tells
    // `sandbox_init` not to allocate an error string (safe in a post-fork child).
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, std::ptr::null_mut()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }
}
