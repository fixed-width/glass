//! RAII guards over Win32 `HGLOBAL` moveable memory, shared by the clipboard read/write paths
//! (`glass-windows` clipboard + the injected `hook`). All `GlobalLock`/`GlobalUnlock`/`GlobalAlloc`/
//! `GlobalFree`/`from_raw_parts` `unsafe` lives here once, behind safe APIs. Windows-only; the crate
//! cross-compiles to `x86_64-pc-windows-gnu` so this is compile-checked on any host.

use core::ffi::c_void;
use std::marker::PhantomData;

use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};

#[cfg(test)]
mod tests;

/// RAII lock over a moveable `HGLOBAL`: `GlobalLock` on construction, `GlobalUnlock` on drop. The
/// byte view is bounded by `GlobalSize`, so reads cannot run past the allocation.
///
/// Borrows — does not own — the handle; freeing is a separate concern (see [`OwnedHGlobal`], or the
/// system taking ownership after `SetClipboardData`).
pub struct HGlobalLock<'a> {
    h: HGLOBAL,
    ptr: *mut c_void,
    len: usize,
    _borrow: PhantomData<&'a [u8]>,
}

impl<'a> HGlobalLock<'a> {
    /// Lock `h`, returning `None` if `GlobalLock` fails (null).
    ///
    /// # Safety
    /// `h` must be a valid `HGLOBAL` whose entire `GlobalSize` is initialized. Its owner must
    /// keep it alive for `'a` and prevent mutation while byte views are live, including through
    /// other locks: `GlobalLock` pins memory but does not provide exclusive access. Prefer [`OwnedHGlobal::lock`] for
    /// allocations owned by Rust; borrowed clipboard data must be tied to the open clipboard.
    pub unsafe fn new(h: HGLOBAL) -> Option<Self> {
        // SAFETY: caller guarantees `h` is valid. GlobalLock pins the moveable block, returning a
        // pointer to it or null on failure.
        let ptr = unsafe { GlobalLock(h) };
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `h` is locked; GlobalSize reports its allocated byte length (0 on error).
        let len = unsafe { GlobalSize(h) };
        let lock = Self {
            h,
            ptr,
            len,
            _borrow: PhantomData,
        };
        // Rust slices cannot exceed isize::MAX, even if the allocator accepts a larger block.
        (len <= isize::MAX as usize).then_some(lock)
    }

    /// The locked block as a byte slice, bounded by `GlobalSize`.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: construction guarantees initialized, immutable storage of `len <= isize::MAX`
        // bytes at the locked non-null base; the slice cannot outlive this lock or its owner.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for HGlobalLock<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard took one lock in `new`; dropping it releases that lock.
        let _ = unsafe { GlobalUnlock(self.h) };
    }
}

/// Owns a `GMEM_MOVEABLE` `HGLOBAL` from `GlobalAlloc`. Frees it in `Drop` unless ownership is
/// relinquished via [`into_raw`](Self::into_raw) (e.g. after `SetClipboardData` takes it).
pub struct OwnedHGlobal {
    h: HGLOBAL,
}

impl OwnedHGlobal {
    /// Allocate a `GMEM_MOVEABLE` block containing `bytes`, with any allocator padding zeroed.
    /// `None` on alloc/lock failure (any partial allocation is freed before returning).
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        // SAFETY: the flags request moveable, initialized storage; Err on failure.
        let h = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) }.ok()?;
        let owned = Self { h };
        {
            // SAFETY: this fresh allocation is initialized and has no aliases. This private lock
            // exposes no byte references while we fill it, and drops before the owner escapes.
            let lock = unsafe { HGlobalLock::new(owned.h) }?;
            if bytes.len() > lock.len {
                return None;
            }
            // SAFETY: the new allocation is disjoint from `bytes`, uniquely accessed, and large
            // enough for this prefix; zero-initialized padding remains unchanged.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), lock.ptr.cast(), bytes.len()) };
        }
        Some(owned)
    }

    /// Borrow the initialized allocation for reading, keeping it alive until the lock drops.
    ///
    /// ```compile_fail
    /// use glass_clip_shim_windows::OwnedHGlobal;
    /// let memory = OwnedHGlobal::from_bytes(b"clipboard").unwrap();
    /// let lock = memory.lock().unwrap();
    /// drop(memory);
    /// assert_eq!(&lock.as_bytes()[..9], b"clipboard");
    /// ```
    pub fn lock(&self) -> Option<HGlobalLock<'_>> {
        // SAFETY: from_bytes initializes the whole allocation; the shared borrow prevents
        // dropping or transferring it, and safe APIs expose no mutable access.
        unsafe { HGlobalLock::new(self.h) }
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
