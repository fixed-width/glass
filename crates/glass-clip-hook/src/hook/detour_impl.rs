//! The `retour` static-detour table for the user32 clipboard APIs.
//!
//! Each detour mirrors the exact Win32 ABI of its export (raw newtypes, `extern "system"`), is
//! resolved to an absolute address via `GetProcAddress` (so *every* call site in the process is
//! intercepted, not just this DLL's IAT slot), and emulates the operation against the host store.
//! The detours NEVER call the real clipboard APIs — there is deliberately no `.call(...)` to the
//! trampoline. That is the whole point: the box also runs `OpenClipboard=n`, so the real APIs are
//! denied; we fully substitute them.
//!
//! Compile-time verified by cross-compiling to `x86_64-pc-windows-gnu`. Whether the detours
//! actually intercept at runtime is finalized on a real Windows box (LOTUS).

use std::cell::{Cell, RefCell};

use retour::static_detour;
use windows::core::BOOL;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ};

use crate::proto::FormatKey;

use super::{
    alloc_hglobal_bytes, id_of, key_of, locale_blob, read_bytes_from_hglobal, store_empty,
    store_get_bytes, store_list, store_set_all, store_seq, unicode_to_codepage, user32_proc,
    CF_BITMAP, CF_DIBV5, CF_LOCALE, CF_OEMTEXT, CF_TEXT,
};

// Exact raw ABI signatures of the user32 exports (from windows-rs `link!` declarations).
// `HANDLE` stands in for the clipboard `HGLOBAL`/`HANDLE` the ABI actually uses (same layout).
type FnOpenClipboard = unsafe extern "system" fn(HWND) -> BOOL;
type FnCloseClipboard = unsafe extern "system" fn() -> BOOL;
type FnEmptyClipboard = unsafe extern "system" fn() -> BOOL;
type FnSetClipboardData = unsafe extern "system" fn(u32, HANDLE) -> HANDLE;
type FnGetClipboardData = unsafe extern "system" fn(u32) -> HANDLE;
type FnIsClipboardFormatAvailable = unsafe extern "system" fn(u32) -> BOOL;
type FnCountClipboardFormats = unsafe extern "system" fn() -> i32;
type FnEnumClipboardFormats = unsafe extern "system" fn(u32) -> u32;
type FnGetClipboardSequenceNumber = unsafe extern "system" fn() -> u32;

static_detour! {
    static OpenClipboardHook: unsafe extern "system" fn(HWND) -> BOOL;
    static CloseClipboardHook: unsafe extern "system" fn() -> BOOL;
    static EmptyClipboardHook: unsafe extern "system" fn() -> BOOL;
    static SetClipboardDataHook: unsafe extern "system" fn(u32, HANDLE) -> HANDLE;
    static GetClipboardDataHook: unsafe extern "system" fn(u32) -> HANDLE;
    static IsClipboardFormatAvailableHook: unsafe extern "system" fn(u32) -> BOOL;
    static CountClipboardFormatsHook: unsafe extern "system" fn() -> i32;
    static EnumClipboardFormatsHook: unsafe extern "system" fn(u32) -> u32;
    static GetClipboardSequenceNumberHook: unsafe extern "system" fn() -> u32;
}

/// What kind of resource the cached handle is, so it is freed via the right Win32 destructor.
#[derive(Clone, Copy, PartialEq)]
enum HandleKind {
    None,
    Global,
    Gdi,
}

thread_local! {
    /// Whether *this thread* currently holds the (emulated) clipboard open. Win32 clipboard
    /// ownership is thread-affine, so per-thread state matches the real semantics and avoids any
    /// cross-thread locking. Purely advisory here — we never gate store access on it (the real
    /// APIs would, but we stay permissive/fail-soft).
    static CLIP_OPEN: Cell<bool> = const { Cell::new(false) };

    /// Formats `SetClipboardData`'d since the last `EmptyClipboard`, shipped as one atomic `SetAll`
    /// at `CloseClipboard` (a multi-format copy = one transaction / one seq bump).
    static PENDING: RefCell<Vec<(FormatKey, Vec<u8>)>> = const { RefCell::new(Vec::new()) };

    /// The last handle returned from `GetClipboardData`. The Win32 contract says the *clipboard*
    /// owns that memory and the app must not free it; we own it instead and free it on the next
    /// open/empty/close (whichever comes first). Tagged by kind so an HGLOBAL is freed via
    /// `GlobalFree` and a GDI HBITMAP via `DeleteObject`. Stored as `isize` (the handle bits) so the
    /// `thread_local!` needs no `unsafe`-Sync dance. A thread-local (not a global Mutex) because
    /// the handle is only ever produced + consumed on the clipboard-owning thread.
    static LAST_HANDLE: Cell<(isize, HandleKind)> = const { Cell::new((0, HandleKind::None)) };
}

/// Free + forget the cached `GetClipboardData` handle, if any. Called on open/empty/close.
fn free_cached_handle() {
    LAST_HANDLE.with(|c| {
        // Zero the cell first so a re-entrant call cannot double-free.
        let (raw, kind) = c.replace((0, HandleKind::None));
        if raw == 0 {
            return;
        }
        match kind {
            // SAFETY: `raw` is an HGLOBAL we allocated in `alloc_hglobal_bytes` (GMEM_MOVEABLE) and
            // handed out exactly once; freeing it here is the agreed ownership transfer.
            HandleKind::Global => unsafe {
                let _ = GlobalFree(Some(HGLOBAL(raw as *mut _)));
            },
            // SAFETY: `raw` is a GDI HBITMAP we created in `make_bitmap_handle` and handed out once;
            // DeleteObject is the matching destructor.
            HandleKind::Gdi => unsafe {
                let _ = DeleteObject(HGDIOBJ(raw as *mut _));
            },
            HandleKind::None => {}
        }
    });
}

/// Cache a freshly-allocated HGLOBAL to be freed (via `GlobalFree`) on the next open/empty/close.
///
/// NOTE: we free the previous handle on the NEXT `GetClipboardData` (via `cache_*` →
/// `free_cached_handle`) as well as on open/empty/close — slightly narrower than the Win32
/// "valid until CloseClipboard" contract, but fine for the lock-copy-unlock pattern apps use.
fn cache_global(h: HGLOBAL) {
    free_cached_handle(); // never leak a prior one
    LAST_HANDLE.with(|c| c.set((h.0 as isize, HandleKind::Global)));
}

/// Cache a freshly-created GDI HBITMAP to be freed (via `DeleteObject`) on the next open/empty/close.
fn cache_bitmap(h: HBITMAP) {
    free_cached_handle(); // never leak a prior one
    LAST_HANDLE.with(|c| c.set((h.0 as isize, HandleKind::Gdi)));
}

// ---- format id <-> FormatKey + synthesis -----------------------------------------------------

/// The clipboard ids currently available to the app: the stored canonical set plus locally
/// synthesizable derivatives (`synth::available`), each mapped to this process's id.
fn available_ids() -> Vec<u32> {
    crate::synth::available(&store_list())
        .iter()
        .map(id_of)
        .filter(|&id| id != 0)
        .collect()
}

/// Bytes for `fmt`: stored verbatim, or synthesized from the canonical stored format.
fn resolve_bytes(fmt: u32, key: &FormatKey) -> Option<Vec<u8>> {
    if let Some(b) = store_get_bytes(key) {
        return Some(b);
    }
    let canon = crate::synth::canonical_for(key)?;
    let src = store_get_bytes(&canon)?;
    Some(match fmt {
        CF_TEXT => unicode_to_codepage(&src, false),
        CF_OEMTEXT => unicode_to_codepage(&src, true),
        CF_LOCALE => locale_blob(),
        CF_DIBV5 => crate::dib::dib_to_dibv5(&src)?,
        CF_BITMAP => src, // handed to make_bitmap_handle by the caller
        _ => return None,
    })
}

/// Build a GDI `HBITMAP` from a validated `CF_DIB` blob. Cached + freed via `DeleteObject`.
fn make_bitmap_handle(dib: &[u8]) -> Option<HANDLE> {
    use windows::Win32::Graphics::Gdi::{
        CreateDIBitmap, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, CBM_INIT, DIB_RGB_COLORS,
    };
    let info = crate::dib::parse_dib(dib)?; // validated geometry → bounds-safe offsets
    // SAFETY: `dib` parsed clean (header + table + bits within bounds). The header ptr is read as a
    // BITMAPINFOHEADER/BITMAPINFO; `bits` = dib + header_bytes + color_table_bytes is in-bounds and
    // sized by `info`. GetDC/ReleaseDC are paired.
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            // SAFETY: ReleaseDC on a null/invalid HDC is a safe no-op; bail rather than hand GDI a null DC.
            ReleaseDC(None, hdc);
            return None;
        }
        let bmih = dib.as_ptr() as *const BITMAPINFOHEADER;
        let bits =
            dib.as_ptr().add(info.header_bytes + info.color_table_bytes) as *const core::ffi::c_void;
        let bmi = dib.as_ptr() as *const BITMAPINFO;
        let hbm = CreateDIBitmap(hdc, Some(bmih), CBM_INIT as u32, Some(bits), Some(bmi), DIB_RGB_COLORS);
        ReleaseDC(None, hdc);
        if hbm.is_invalid() {
            None
        } else {
            cache_bitmap(hbm);
            Some(HANDLE(hbm.0))
        }
    }
}

// ---- detour bodies ---------------------------------------------------------------------------
//
// Every detour body is wrapped in `catch_unwind`: a Rust panic unwinding across an `extern
// "system"` frame into the host app is UB. `catch_unwind` contains it and we return the same
// fail-soft default the body uses for a down pipe. (A member-crate `panic = "abort"` would NOT
// help — Cargo only honors `[profile]` at the workspace root, so it is silently ignored.)
// `AssertUnwindSafe` is sound here: on panic we return a safe default and mutate no shared
// invariant, even though the closures touch raw pointers / thread-local `Cell`s.

/// `OpenClipboard(hwnd)` → mark open; always succeed. Never touches the real clipboard.
fn open_clipboard(_hwnd: HWND) -> BOOL {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        free_cached_handle();
        CLIP_OPEN.with(|c| c.set(true));
        BOOL(1)
    }))
    .unwrap_or(BOOL(0))
}

/// `CloseClipboard()` → mark closed; commit the pending multi-format copy as one atomic `SetAll`
/// (one transaction / one seq bump); free the cached handle. Always succeed.
fn close_clipboard() -> BOOL {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        CLIP_OPEN.with(|c| c.set(false));
        let items = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
        if !items.is_empty() {
            store_set_all(items);
        }
        free_cached_handle();
        BOOL(1)
    }))
    .unwrap_or(BOOL(0))
}

/// `EmptyClipboard()` → drop any pending copy, clear the host store, free the cached handle.
fn empty_clipboard() -> BOOL {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        PENDING.with(|p| p.borrow_mut().clear());
        store_empty();
        free_cached_handle();
        BOOL(1)
    }))
    .unwrap_or(BOOL(0))
}

/// `SetClipboardData(fmt, h)` → capture the format's bytes from the app's `HGLOBAL` into the pending
/// set (committed atomically on `CloseClipboard`). Returns `h` (the contract: the clipboard now
/// "owns" it; we keep the store as the source of truth). `h == NULL` is delayed rendering — out of
/// scope (OLE apps covered separately in 2a-ii): skip, no change.
fn set_clipboard_data(fmt: u32, h: HANDLE) -> HANDLE {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if h.0.is_null() {
            return HANDLE::default(); // delayed rendering unsupported
        }
        if let Some(bytes) = read_bytes_from_hglobal(HGLOBAL(h.0)) {
            let key = key_of(fmt);
            PENDING.with(|p| {
                let mut v = p.borrow_mut();
                v.retain(|(k, _)| *k != key); // last write wins per format
                v.push((key, bytes));
            });
        }
        h
    }))
    .unwrap_or(HANDLE::default())
}

/// `GetClipboardData(fmt)` → resolve the format's bytes (stored verbatim or synthesized), then hand
/// back a handle the app needn't free (we free on next open/empty/close): a GDI `HBITMAP` for
/// `CF_BITMAP`, otherwise a fresh `HGLOBAL`. NULL if unavailable or the pipe is down.
fn get_clipboard_data(fmt: u32) -> HANDLE {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let key = key_of(fmt);
        let Some(bytes) = resolve_bytes(fmt, &key) else {
            return HANDLE::default();
        };
        if fmt == CF_BITMAP {
            return make_bitmap_handle(&bytes).unwrap_or_default();
        }
        match alloc_hglobal_bytes(&bytes) {
            Some(h) => {
                cache_global(h);
                HANDLE(h.0)
            }
            None => HANDLE::default(),
        }
    }))
    .unwrap_or(HANDLE::default())
}

/// `IsClipboardFormatAvailable(fmt)` → TRUE iff `fmt` is among the stored/synthesizable formats.
fn is_clipboard_format_available(fmt: u32) -> BOOL {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BOOL(available_ids().contains(&fmt) as i32)
    }))
    .unwrap_or(BOOL(0))
}

/// `CountClipboardFormats()` → the number of stored/synthesizable formats.
fn count_clipboard_formats() -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| available_ids().len() as i32))
        .unwrap_or(0)
}

/// `EnumClipboardFormats(prev)` → walk the stored/synthesizable formats in order: `0` yields the
/// first; otherwise the one after `prev`; `0` again at the end of the list.
fn enum_clipboard_formats(prev: u32) -> u32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ids = available_ids();
        match prev {
            0 => ids.first().copied().unwrap_or(0),
            _ => ids
                .iter()
                .position(|&x| x == prev)
                .and_then(|i| ids.get(i + 1))
                .copied()
                .unwrap_or(0),
        }
    }))
    .unwrap_or(0)
}

/// `GetClipboardSequenceNumber()` → the host store's sequence number (bumps on every set/empty).
fn get_clipboard_sequence_number() -> u32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(store_seq)).unwrap_or(0)
}

// ---- installation ----------------------------------------------------------------------------

/// Resolve every user32 clipboard export and enable its detour. Best-effort + fail-soft: a single
/// unresolved/uninitialisable export is logged-by-omission and skipped, never panicking (a panic
/// across the FFI boundary in `InjectDllMain` would take down the boxed app).
pub(super) fn install() {
    // SAFETY (whole block): each `user32_proc` returns the absolute address of the named export,
    // which we transmute to that export's exact ABI signature (verified against windows-rs `link!`
    // declarations). `retour`'s `initialize` then trampolines the target and routes calls to our
    // hook; `enable` patches the prologue. Both are `unsafe` by contract. Every step is fallible
    // and we `let _ =` / `if let` to stay fail-soft — we never unwrap across the FFI boundary.
    unsafe {
        if let Some(p) = user32_proc(b"OpenClipboard\0") {
            let target: FnOpenClipboard = std::mem::transmute(p);
            if OpenClipboardHook.initialize(target, open_clipboard).is_ok() {
                let _ = OpenClipboardHook.enable();
            }
        }
        if let Some(p) = user32_proc(b"CloseClipboard\0") {
            let target: FnCloseClipboard = std::mem::transmute(p);
            if CloseClipboardHook.initialize(target, close_clipboard).is_ok() {
                let _ = CloseClipboardHook.enable();
            }
        }
        if let Some(p) = user32_proc(b"EmptyClipboard\0") {
            let target: FnEmptyClipboard = std::mem::transmute(p);
            if EmptyClipboardHook.initialize(target, empty_clipboard).is_ok() {
                let _ = EmptyClipboardHook.enable();
            }
        }
        if let Some(p) = user32_proc(b"SetClipboardData\0") {
            let target: FnSetClipboardData = std::mem::transmute(p);
            if SetClipboardDataHook
                .initialize(target, set_clipboard_data)
                .is_ok()
            {
                let _ = SetClipboardDataHook.enable();
            }
        }
        if let Some(p) = user32_proc(b"GetClipboardData\0") {
            let target: FnGetClipboardData = std::mem::transmute(p);
            if GetClipboardDataHook
                .initialize(target, get_clipboard_data)
                .is_ok()
            {
                let _ = GetClipboardDataHook.enable();
            }
        }
        if let Some(p) = user32_proc(b"IsClipboardFormatAvailable\0") {
            let target: FnIsClipboardFormatAvailable = std::mem::transmute(p);
            if IsClipboardFormatAvailableHook
                .initialize(target, is_clipboard_format_available)
                .is_ok()
            {
                let _ = IsClipboardFormatAvailableHook.enable();
            }
        }
        if let Some(p) = user32_proc(b"CountClipboardFormats\0") {
            let target: FnCountClipboardFormats = std::mem::transmute(p);
            if CountClipboardFormatsHook
                .initialize(target, count_clipboard_formats)
                .is_ok()
            {
                let _ = CountClipboardFormatsHook.enable();
            }
        }
        if let Some(p) = user32_proc(b"EnumClipboardFormats\0") {
            let target: FnEnumClipboardFormats = std::mem::transmute(p);
            if EnumClipboardFormatsHook
                .initialize(target, enum_clipboard_formats)
                .is_ok()
            {
                let _ = EnumClipboardFormatsHook.enable();
            }
        }
        if let Some(p) = user32_proc(b"GetClipboardSequenceNumber\0") {
            let target: FnGetClipboardSequenceNumber = std::mem::transmute(p);
            if GetClipboardSequenceNumberHook
                .initialize(target, get_clipboard_sequence_number)
                .is_ok()
            {
                let _ = GetClipboardSequenceNumberHook.enable();
            }
        }
    }
    super::ole::install_ole();
}
