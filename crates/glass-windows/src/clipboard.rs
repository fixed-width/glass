//! Win32 clipboard get/set via `CF_UNICODETEXT`.

use std::marker::PhantomData;
use std::sync::{Mutex, MutexGuard, TryLockError};

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use glass_clip_shim_windows::{HGlobalLock, OwnedHGlobal};
use glass_core::{GlassError, Result};

#[cfg(test)]
mod tests;

// OpenClipboard(None) can succeed again on another thread in this process.
static CLIPBOARD_ACCESS: Mutex<()> = Mutex::new(());

/// Read the clipboard as UTF-8 text.
///
/// Returns `Ok("")` when the clipboard is empty or contains no text (no
/// `CF_UNICODETEXT` owner). Maps Win32 failures to [`GlassError::Backend`].
pub fn get() -> Result<String> {
    Clipboard::open()?.read_text()
}

/// Write UTF-8 text to the clipboard (encoded as NUL-terminated UTF-16).
pub fn set(text: &str) -> Result<()> {
    let bytes: Vec<u8> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    let memory = OwnedHGlobal::from_bytes(&bytes)
        .ok_or_else(|| GlassError::Backend("GlobalAlloc/GlobalLock failed for clipboard".into()))?;
    Clipboard::open()?.write_text(memory)
}

/// Holds the clipboard open on this thread; borrowed data must be unlocked before it closes.
struct Clipboard {
    _access: MutexGuard<'static, ()>,
    _thread_bound: PhantomData<*mut ()>,
}

impl Clipboard {
    fn open() -> Result<Self> {
        let access = match CLIPBOARD_ACCESS.try_lock() {
            Ok(access) => access,
            // There is no protected data to repair; the previous guard closed during unwinding.
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(GlassError::Backend(
                    "OpenClipboard failed: clipboard already open in this process".into(),
                ));
            }
        };
        // SAFETY: no window is associated with this task's clipboard access.
        unsafe { OpenClipboard(None) }
            .map_err(|e| GlassError::Backend(format!("OpenClipboard failed: {e}")))?;
        Ok(Self {
            _access: access,
            _thread_bound: PhantomData,
        })
    }

    fn lock_text(&self) -> Result<Option<HGlobalLock<'_>>> {
        // SAFETY: this guard holds the clipboard open; CF_UNICODETEXT data is a borrowed HGLOBAL.
        let handle = match unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) } {
            Ok(handle) if !handle.is_invalid() => handle,
            _ => return Ok(None),
        };
        // SAFETY: the clipboard owns the text bytes and prevents concurrent modification while
        // open; the returned lock borrows this guard, preventing closure or write_text until drop.
        unsafe { HGlobalLock::new(HGLOBAL(handle.0)) }
            .map(Some)
            .ok_or_else(|| GlassError::Backend("GlobalLock failed on clipboard handle".into()))
    }

    fn read_text(&self) -> Result<String> {
        let Some(lock) = self.lock_text()? else {
            return Ok(String::new());
        };
        // An external owner may omit the NUL terminator or leave an odd trailing byte.
        let units: Vec<u16> = lock
            .as_bytes()
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        Ok(String::from_utf16_lossy(&units))
    }

    fn write_text(&mut self, memory: OwnedHGlobal) -> Result<()> {
        // SAFETY: the clipboard is open; the exclusive borrow excludes outstanding data locks.
        unsafe { EmptyClipboard() }
            .map_err(|e| GlassError::Backend(format!("EmptyClipboard failed: {e}")))?;
        // SAFETY: the clipboard is open and empty; the initialized block is unlocked. Windows
        // takes ownership only on success; otherwise `memory` frees the allocation on return.
        unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.handle().0))) }
            .map_err(|e| GlassError::Backend(format!("SetClipboardData failed: {e}")))?;
        memory.into_raw();
        Ok(())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: open succeeded on this thread; all borrowed data locks have ended.
        let _ = unsafe { CloseClipboard() };
    }
}
