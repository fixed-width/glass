//! Win32 clipboard get/set via `CF_UNICODETEXT`.
//!
//! The Windows clipboard is global OS storage — there is no owner thread and nothing
//! to tear down. Both functions are safe wrappers around `unsafe` Win32 calls; every
//! `unsafe` block is guarded by a `// SAFETY:` comment per the repo policy.

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use glass_clip_hook::{HGlobalLock, OwnedHGlobal};
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

    // Reinterpret the clipboard HANDLE as an HGLOBAL (both wrap `*mut c_void`;
    // GetClipboardData returns an HGLOBAL for CF_UNICODETEXT).
    let hglobal = HGLOBAL(handle.0);

    // SAFETY: `hglobal` is the HGLOBAL GetClipboardData returned for CF_UNICODETEXT;
    // it stays valid until the caller closes the clipboard, which outlives this `lock`.
    let lock = unsafe { HGlobalLock::new(hglobal) }
        .ok_or_else(|| GlassError::Backend("GlobalLock failed on clipboard handle".into()))?;

    // The clipboard owner is an arbitrary (possibly buggy/malicious) app, so CF_UNICODETEXT
    // may not be NUL-terminated. Bound the scan by the locked block size; `chunks_exact` drops
    // any stray trailing odd byte (valid UTF-16 is even-length). `lock` unlocks on drop.
    let units: Vec<u16> = lock
        .as_bytes()
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

/// Write UTF-8 text to the clipboard (encoded as NUL-terminated UTF-16).
///
/// On success the system owns the allocated global memory — we must NOT free it. On any failure the
/// `OwnedHGlobal` frees the allocation as it drops.
pub fn set(text: &str) -> Result<()> {
    // Encode as native-endian (Windows = little-endian) UTF-16 + NUL terminator, as raw bytes.
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
    let mut bytes = Vec::with_capacity(utf16.len() * 2);
    for unit in utf16 {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

    // `mem` frees itself on drop until we relinquish it to the system via `into_raw` below.
    let mem = OwnedHGlobal::from_bytes(&bytes)
        .ok_or_else(|| GlassError::Backend("GlobalAlloc/GlobalLock failed for clipboard".into()))?;

    // SAFETY: OpenClipboard(None) is safe from any thread. On the `?` early-return `mem` drops -> free.
    unsafe { OpenClipboard(None) }
        .map_err(|e| GlassError::Backend(format!("OpenClipboard failed: {e}")))?;

    // SAFETY: the clipboard is open; EmptyClipboard clears it and transfers ownership to us.
    if let Err(e) = unsafe { EmptyClipboard() } {
        // SAFETY: we opened the clipboard above. `mem` drops after this -> GlobalFree.
        let _ = unsafe { CloseClipboard() };
        return Err(GlassError::Backend(format!("EmptyClipboard failed: {e}")));
    }

    // HGLOBAL -> HANDLE for SetClipboardData (both wrap `*mut c_void`).
    let hmem = HANDLE(mem.handle().0);

    // SAFETY: the clipboard is open and empty. On success SetClipboardData transfers ownership of the
    // block to the system (so we must NOT free it — `into_raw`); on failure it stays ours.
    let set_result = unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(hmem)) };

    // SAFETY: the clipboard was opened; close it regardless of the set result.
    let _ = unsafe { CloseClipboard() };

    match set_result {
        Ok(_) => {
            mem.into_raw(); // system owns the block now — suppress the Drop free
            Ok(())
        }
        // `mem` drops here -> GlobalFree (ownership never transferred).
        Err(e) => Err(GlassError::Backend(format!("SetClipboardData failed: {e}"))),
    }
}
