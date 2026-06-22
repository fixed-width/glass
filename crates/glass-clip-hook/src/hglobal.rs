//! RAII guards over Win32 `HGLOBAL` moveable memory, shared by the clipboard read/write paths
//! (`glass-windows` clipboard + the injected `hook`). All `GlobalLock`/`GlobalUnlock`/`GlobalAlloc`/
//! `GlobalFree`/`from_raw_parts` `unsafe` lives here once, behind safe APIs. Windows-only; the crate
//! cross-compiles to `x86_64-pc-windows-gnu` so this is compile-checked on the Linux dev box.

use core::ffi::c_void;

use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

/// RAII lock over a moveable `HGLOBAL`: `GlobalLock` on construction, `GlobalUnlock` on drop. The
/// byte view is bounded by `GlobalSize`, so reads cannot run past the allocation.
///
/// Borrows — does not own — the handle; freeing is a separate concern (see [`OwnedHGlobal`], or the
/// system taking ownership after `SetClipboardData`).
pub struct HGlobalLock {
    h: HGLOBAL,
    ptr: *mut c_void,
    len: usize,
}

impl HGlobalLock {
    /// Lock `h`, returning `None` if `GlobalLock` fails (null).
    ///
    /// # Safety
    /// `h` must be a valid `HGLOBAL` (from `GlobalAlloc`, or a clipboard data handle for a global
    /// format) that stays valid for the lifetime of the returned guard.
    pub unsafe fn new(h: HGLOBAL) -> Option<Self> {
        // SAFETY: caller guarantees `h` is valid. GlobalLock pins the moveable block, returning a
        // pointer to it or null on failure.
        let ptr = unsafe { GlobalLock(h) };
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `h` is locked; GlobalSize reports its allocated byte length (0 on error).
        let len = unsafe { GlobalSize(h) };
        Some(Self { h, ptr, len })
    }

    /// The locked block as a byte slice, bounded by `GlobalSize`.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is the locked, non-null base; `len` is the GlobalSize byte length. The slice
        // borrows `self`, so it cannot outlive the lock that keeps the block pinned.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }

    /// The locked block as a mutable byte slice. Internal: only [`OwnedHGlobal::from_bytes`] writes.
    pub(crate) fn as_mut_bytes(&mut self) -> &mut [u8] {
        // SAFETY: as `as_bytes`; `&mut self` proves we hold the only reference to the block.
        unsafe { std::slice::from_raw_parts_mut(self.ptr as *mut u8, self.len) }
    }
}

impl Drop for HGlobalLock {
    fn drop(&mut self) {
        // SAFETY: we took the matching lock in `new`. For a GMEM_MOVEABLE block the result is
        // informational (Err only when the lock count reaches 0), so we ignore it.
        let _ = unsafe { GlobalUnlock(self.h) };
    }
}

/// Owns a `GMEM_MOVEABLE` `HGLOBAL` from `GlobalAlloc`. Frees it in `Drop` unless ownership is
/// relinquished via [`into_raw`](Self::into_raw) (e.g. after `SetClipboardData` takes it).
pub struct OwnedHGlobal {
    h: HGLOBAL,
}

impl OwnedHGlobal {
    /// Allocate a `GMEM_MOVEABLE` block holding exactly `bytes`. `None` on alloc/lock failure (any
    /// partial allocation is freed before returning).
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        // SAFETY: GlobalAlloc(GMEM_MOVEABLE, n) is the canonical moveable allocation; Err on failure.
        let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }.ok()?;
        let owned = Self { h };
        {
            // SAFETY: `h` was just returned by GlobalAlloc, so it is valid.
            let mut lock = unsafe { HGlobalLock::new(owned.h) }?; // on None, `owned` drops -> free
            // GlobalAlloc may round the block UP, so the locked slice can be longer than requested;
            // copy into the exact prefix (a whole-slice copy_from_slice would panic on a mismatch).
            lock.as_mut_bytes()[..bytes.len()].copy_from_slice(bytes);
            // `lock` drops here -> GlobalUnlock.
        }
        Some(owned)
    }

    /// The raw handle, for APIs that need it (e.g. `HANDLE(h.0)` for `SetClipboardData`).
    pub fn handle(&self) -> HGLOBAL {
        self.h
    }

    /// Relinquish ownership: return the raw handle and suppress the `Drop` free. Call after the
    /// system takes the block (e.g. `SetClipboardData` succeeded) or to hand it to a caller that
    /// will free it.
    pub fn into_raw(self) -> HGLOBAL {
        let h = self.h;
        std::mem::forget(self);
        h
    }
}

impl Drop for OwnedHGlobal {
    fn drop(&mut self) {
        // SAFETY: we own `self.h` (from GlobalAlloc, not relinquished via into_raw), so GlobalFree
        // is the correct release. Result is informational.
        let _ = unsafe { GlobalFree(Some(self.h)) };
    }
}
