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

use std::cell::Cell;

use retour::static_detour;
use windows::Win32::Foundation::{GlobalFree, BOOL, HANDLE, HGLOBAL, HWND};

use super::{
    alloc_hglobal_for, is_text_format, read_singlebyte_from_hglobal, read_utf16_from_hglobal,
    store_empty, store_get, store_seq, store_set, user32_proc, CF_OEMTEXT_ID, CF_TEXT_ID,
    CF_UNICODETEXT_ID,
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

thread_local! {
    /// Whether *this thread* currently holds the (emulated) clipboard open. Win32 clipboard
    /// ownership is thread-affine, so per-thread state matches the real semantics and avoids any
    /// cross-thread locking. Purely advisory here — we never gate store access on it (the real
    /// APIs would, but we stay permissive/fail-soft).
    static CLIP_OPEN: Cell<bool> = const { Cell::new(false) };

    /// The last `HGLOBAL` returned from `GetClipboardData`. The Win32 contract says the *clipboard*
    /// owns that memory and the app must not free it; we own it instead and free it on the next
    /// open/empty/close (whichever comes first). Stored as `isize` (the pointer bits) so the
    /// `thread_local!` needs no `unsafe`-Sync dance. A thread-local (not a global Mutex) because
    /// the handle is only ever produced + consumed on the clipboard-owning thread.
    static LAST_HANDLE: Cell<isize> = const { Cell::new(0) };
}

/// Free + forget the cached `GetClipboardData` handle, if any. Called on open/empty/close.
fn free_cached_handle() {
    LAST_HANDLE.with(|c| {
        let raw = c.replace(0);
        if raw != 0 {
            // SAFETY: `raw` is an HGLOBAL we allocated in `alloc_hglobal_for` (GMEM_MOVEABLE) and
            // handed out exactly once; freeing it here is the agreed ownership transfer. We zero
            // the cell first so a re-entrant call cannot double-free.
            unsafe {
                let _ = GlobalFree(HGLOBAL(raw as *mut _));
            }
        }
    });
}

/// Cache a freshly-allocated handle to be freed on the next open/empty/close.
///
/// NOTE: we free the previous handle on the NEXT `GetClipboardData` (via `cache_handle` →
/// `free_cached_handle`) as well as on open/empty/close — slightly narrower than the Win32
/// "valid until CloseClipboard" contract, but fine for the lock-copy-unlock pattern apps use.
fn cache_handle(h: HGLOBAL) {
    free_cached_handle(); // never leak a prior one
    LAST_HANDLE.with(|c| c.set(h.0 as isize));
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

/// `CloseClipboard()` → mark closed; free the cached handle. Always succeed.
fn close_clipboard() -> BOOL {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        CLIP_OPEN.with(|c| c.set(false));
        free_cached_handle();
        BOOL(1)
    }))
    .unwrap_or(BOOL(0))
}

/// `EmptyClipboard()` → clear the host store; free the cached handle. Always succeed.
fn empty_clipboard() -> BOOL {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store_empty();
        free_cached_handle();
        BOOL(1)
    }))
    .unwrap_or(BOOL(0))
}

/// `SetClipboardData(fmt, h)` → for text formats with a non-NULL handle, read the text in the
/// format's own encoding and store it. Returns `h` (the contract: the clipboard now "owns" it; we
/// keep the store as the source of truth). `h == NULL` is delayed rendering — out of scope in v1:
/// return NULL, no change. Non-text formats are ignored (return the handle unchanged).
///
/// v1 text-format handling: `CF_UNICODETEXT` reads as UTF-16; `CF_TEXT`/`CF_OEMTEXT` read as
/// single-byte ANSI (the app's buffer is single-byte — reading it as UTF-16 would garble it).
fn set_clipboard_data(fmt: u32, h: HANDLE) -> HANDLE {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if h.0.is_null() {
            return HANDLE::default(); // delayed rendering unsupported (v1)
        }
        let text = if fmt == CF_UNICODETEXT_ID {
            read_utf16_from_hglobal(HGLOBAL(h.0))
        } else if fmt == CF_TEXT_ID || fmt == CF_OEMTEXT_ID {
            read_singlebyte_from_hglobal(HGLOBAL(h.0))
        } else {
            None // non-text format: ignore
        };
        if let Some(text) = text {
            store_set(&text);
        }
        h
    }))
    .unwrap_or(HANDLE::default())
}

/// `GetClipboardData(fmt)` → for text formats, allocate a fresh `HGLOBAL` in the requested encoding
/// from the host store, cache it (so the app needn't free it; we free on next open/empty/close),
/// and return it. NULL if the store is empty, the format is non-text, or the pipe is down.
fn get_clipboard_data(fmt: u32) -> HANDLE {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !is_text_format(fmt) {
            return HANDLE::default();
        }
        let Some(text) = store_get() else {
            return HANDLE::default();
        };
        match alloc_hglobal_for(fmt, &text) {
            Some(h) => {
                cache_handle(h);
                HANDLE(h.0)
            }
            None => HANDLE::default(),
        }
    }))
    .unwrap_or(HANDLE::default())
}

/// `IsClipboardFormatAvailable(fmt)` → TRUE iff a text format and the store is non-empty.
fn is_clipboard_format_available(fmt: u32) -> BOOL {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BOOL((is_text_format(fmt) && store_get().is_some()) as i32)
    }))
    .unwrap_or(BOOL(0))
}

/// `CountClipboardFormats()` → 2 (CF_UNICODETEXT + CF_TEXT) when the store has text, else 0.
fn count_clipboard_formats() -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if store_get().is_some() {
            2
        } else {
            0
        }
    }))
    .unwrap_or(0)
}

/// `EnumClipboardFormats(prev)` → enumerate our synthesized text formats:
/// 0 → CF_UNICODETEXT; CF_UNICODETEXT → CF_TEXT; anything else → 0 (end of list).
fn enum_clipboard_formats(prev: u32) -> u32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match prev {
        0 => CF_UNICODETEXT_ID,
        CF_UNICODETEXT_ID => CF_TEXT_ID,
        _ => 0,
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
}
