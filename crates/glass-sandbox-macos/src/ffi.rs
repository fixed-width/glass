//! `sandbox_init` FFI. `apply` is meant to run inside `Command::pre_exec` (post-fork,
//! pre-exec), so it does NO allocation: the caller builds the `CString` first and this makes
//! a single syscall, returning an alloc-free `io::Error` on failure. `sandbox_init` accepts
//! an inline SBPL string with flag `0`; it is deprecated-but-shipping (Apple + Chromium).

use std::ffi::{c_char, c_int, CStr, CString};

use glass_core::{GlassError, Result};

extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Apply the SBPL `profile` to the current process. Intended for `pre_exec`.
///
/// NOTE: to stay fork-safe, prefer [`apply_cstr`] from inside a `pre_exec` closure (it does
/// no allocation). `apply` is the convenience entry for non-fork contexts (tests, tooling).
pub fn apply(profile: &str) -> Result<()> {
    let c = CString::new(profile)
        .map_err(|e| GlassError::SandboxUnavailable(format!("profile contains NUL: {e}")))?;
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: `c` is a valid NUL-terminated C string for the duration of the call; `err` is a
    // valid out-pointer. `sandbox_init` reads the profile and, on failure, allocates an error
    // string we free below with `sandbox_free_error`.
    let rc = unsafe { sandbox_init(c.as_ptr(), 0, &mut err) };
    if rc == 0 {
        return Ok(());
    }
    let msg = if err.is_null() {
        "sandbox_init failed".to_string()
    } else {
        // SAFETY: on failure `sandbox_init` set `err` to a heap C string; read then free it.
        let s = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        unsafe { sandbox_free_error(err) };
        s
    };
    Err(GlassError::SandboxUnavailable(format!("sandbox_init: {msg}")))
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
