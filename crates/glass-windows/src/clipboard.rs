//! Win32 clipboard get/set via `CF_UNICODETEXT`.
//!
//! The Windows clipboard is global OS storage — there is no owner thread and nothing
//! to tear down. Both functions are safe wrappers around `unsafe` Win32 calls; every
//! `unsafe` block is guarded by a `// SAFETY:` comment per the repo policy.

use std::ptr;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use glass_core::{GlassError, Result};

/// Read the clipboard as UTF-8 text.
///
/// Returns `Ok("")` when the clipboard is empty or contains no text (no
/// `CF_UNICODETEXT` owner). Maps Win32 failures to [`GlassError::Backend`].
pub fn get() -> Result<String> {
    // SAFETY: OpenClipboard(None) is safe to call from any thread; None means
    // "associate with no window" which is the documented practice for non-GUI code.
    unsafe { OpenClipboard(None) }
        .map_err(|e| GlassError::Backend(format!("OpenClipboard failed: {e}")))?;

    // Always close, even on the empty/error paths — use a scope guard via a local
    // wrapper.  We handle the close manually at each return site to keep the
    // `unsafe` blocks minimal.

    let result = read_clipboard_text();

    // SAFETY: We successfully opened the clipboard above; CloseClipboard must be
    // called exactly once to release the lock.  We call it unconditionally here
    // (after the read) so it always runs.
    let _ = unsafe { CloseClipboard() };

    result
}

/// Inner: reads `CF_UNICODETEXT` while the clipboard is open.  Called only
/// after a successful `OpenClipboard`; the caller owns the close.
fn read_clipboard_text() -> Result<String> {
    // SAFETY: The clipboard is open (caller guarantee).  GetClipboardData returns
    // a handle owned by the clipboard — we must NOT free it.  A null/error result
    // means no CF_UNICODETEXT data is available, which is `Ok("")`.
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) };

    let handle = match handle {
        Ok(h) if !h.is_invalid() => h,
        // No text data on the clipboard — this is not an error.
        _ => return Ok(String::new()),
    };

    // Reinterpret the clipboard HANDLE as an HGLOBAL (both are `*mut c_void`
    // wrappers; Win32 GetClipboardData returns an HGLOBAL for `CF_UNICODETEXT`).
    let hglobal = HGLOBAL(handle.0);

    // SAFETY: `hglobal` is a valid HGLOBAL returned by GetClipboardData for
    // CF_UNICODETEXT while the clipboard is open.  GlobalLock returns a pointer
    // to the moveable memory; we hold no other references to this block.
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return Err(GlassError::Backend("GlobalLock failed on clipboard handle".into()));
    }

    // Read the NUL-terminated UTF-16 string.
    let text = {
        // SAFETY: `ptr` is valid UTF-16 data locked from the clipboard handle.
        // We walk until the NUL terminator, which CF_UNICODETEXT data must have.
        let wchars: *const u16 = ptr as *const u16;
        let mut len = 0usize;
        unsafe {
            while *wchars.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(wchars, len);
            String::from_utf16_lossy(slice)
        }
    };

    // SAFETY: We locked hglobal above; we must unlock it before the clipboard is
    // closed.  GlobalUnlock returns an error only when the lock count hits 0 for a
    // GMEM_MOVEABLE block — for our read path the result is informational only, so
    // we ignore it.
    let _ = unsafe { GlobalUnlock(hglobal) };

    Ok(text)
}

/// Write UTF-8 text to the clipboard (encoded as NUL-terminated UTF-16).
///
/// On success the system owns the allocated global memory — we must NOT free it.
/// On failure we free the allocation and return [`GlassError::Backend`].
pub fn set(text: &str) -> Result<()> {
    // Encode as UTF-16 + NUL terminator.
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
    let byte_len = utf16.len() * std::mem::size_of::<u16>();

    // SAFETY: GlobalAlloc with GMEM_MOVEABLE is the canonical way to allocate a
    // moveable block for the clipboard.  `byte_len` is always >0 (at minimum the
    // NUL terminator).
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) }
        .map_err(|e| GlassError::Backend(format!("GlobalAlloc failed: {e}")))?;

    // Copy the UTF-16 data into the locked block.
    {
        // SAFETY: `hglobal` is a valid GMEM_MOVEABLE handle just returned by
        // GlobalAlloc.  GlobalLock returns a writable pointer to the block.
        let ptr = unsafe { GlobalLock(hglobal) };
        if ptr.is_null() {
            // Free on this error path — we still own the memory.
            // SAFETY: We own hglobal (GlobalAlloc succeeded, SetClipboardData has
            // not been called yet), so GlobalFree is safe.
            let _ = unsafe { GlobalFree(hglobal) };
            return Err(GlassError::Backend("GlobalLock failed on new clipboard handle".into()));
        }
        // SAFETY: `ptr` points to `byte_len` bytes of writable memory; `utf16`
        // has exactly `byte_len` bytes.  No aliasing — we hold the only reference.
        unsafe {
            ptr::copy_nonoverlapping(utf16.as_ptr() as *const u8, ptr as *mut u8, byte_len);
        }
        // SAFETY: We locked hglobal above; unlock before SetClipboardData.
        let _ = unsafe { GlobalUnlock(hglobal) };
    }

    // SAFETY: OpenClipboard(None) is safe from any thread (no owner window needed).
    unsafe { OpenClipboard(None) }
        .map_err(|e| {
            // We still own hglobal — free it before returning.
            // SAFETY: SetClipboardData has not been called, so we still own hglobal.
            let _ = unsafe { GlobalFree(hglobal) };
            GlassError::Backend(format!("OpenClipboard failed: {e}"))
        })?;

    // SAFETY: The clipboard is now open.  EmptyClipboard clears existing data and
    // transfers clipboard ownership to us.
    let empty_result = unsafe { EmptyClipboard() };
    if let Err(e) = empty_result {
        // SAFETY: We still own hglobal; free it before closing.
        let _ = unsafe { GlobalFree(hglobal) };
        // SAFETY: We successfully opened the clipboard.
        let _ = unsafe { CloseClipboard() };
        return Err(GlassError::Backend(format!("EmptyClipboard failed: {e}")));
    }

    // Convert HGLOBAL to HANDLE for SetClipboardData (both are `*mut c_void`
    // wrappers; SetClipboardData's second parameter is typed as HANDLE).
    let hmem = HANDLE(hglobal.0);

    // SAFETY: The clipboard is open and empty.  SetClipboardData transfers ownership
    // of `hmem`/`hglobal` to the system on success — we must NOT free it afterwards.
    // On failure the allocation is still ours and we free it below.
    let set_result = unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, hmem) };

    // SAFETY: The clipboard was successfully opened; close it regardless of whether
    // SetClipboardData succeeded.
    let _ = unsafe { CloseClipboard() };

    match set_result {
        Ok(_) => Ok(()),
        Err(e) => {
            // SetClipboardData failed — we still own hglobal; free it.
            // SAFETY: SetClipboardData failed, so ownership did not transfer; we must
            // free to avoid a leak.
            let _ = unsafe { GlobalFree(hglobal) };
            Err(GlassError::Backend(format!("SetClipboardData failed: {e}")))
        }
    }
}
