//! user32 clipboard detours + the pipe client (cfg windows; runtime-validated on LOTUS).
//!
//! Emulates a per-app clipboard backed by the host store. The detours NEVER call the real
//! clipboard APIs (the box also has `OpenClipboard=n`), so the user's clipboard is untouched.
//! v1: `CF_UNICODETEXT` (+ `CF_TEXT`/`CF_OEMTEXT` synthesized on read). Eager data only — delayed
//! rendering (`SetClipboardData(_, NULL)`) and OLE are out of scope (documented limitation).
//!
//! Compile-time verified by cross-compiling to `x86_64-pc-windows-gnu`. Runtime interception is
//! finalized on a real Windows box (LOTUS); the detour table is authored complete so that on-box
//! work is debugging, not authoring.

use std::sync::OnceLock;

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HGLOBAL};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use crate::proto::{self, Request, Response};

mod detour_impl;

// CF_UNICODETEXT = 13, CF_TEXT = 1, CF_OEMTEXT = 7 (stable Win32 ABI ids).
pub(crate) const CF_UNICODETEXT_ID: u32 = 13;
pub(crate) const CF_TEXT_ID: u32 = 1;
pub(crate) const CF_OEMTEXT_ID: u32 = 7;

/// Resolved pipe path (`\\.\pipe\<GLASS_CLIP_PIPE>`). `None` ⇒ the DLL is inert.
static PIPE: OnceLock<String> = OnceLock::new();

/// True iff a text clipboard format. The three synthesized text formats are interchangeable on
/// read (we down/up-convert UTF-16); only `CF_UNICODETEXT` is stored.
pub(crate) fn is_text_format(fmt: u32) -> bool {
    fmt == CF_UNICODETEXT_ID || fmt == CF_TEXT_ID || fmt == CF_OEMTEXT_ID
}

/// Called from `InjectDllMain`. Inert if `GLASS_CLIP_PIPE` is unset (only the target app's
/// process tree carries it; every other boxed process loads this DLL but stays dormant).
pub(crate) fn init() {
    let Ok(name) = std::env::var("GLASS_CLIP_PIPE") else {
        return;
    };
    if name.is_empty() {
        return;
    }
    let _ = PIPE.set(format!(r"\\.\pipe\{name}"));
    detour_impl::install();
}

// ---------------------------------------------------------------------------------------------
// Pipe client: one request → one response over a fresh connection. Clipboard ops are rare, so
// there is no pooling. Every failure path returns `None` / does nothing (fail-soft): the detours
// then behave as an empty clipboard rather than panicking or touching the real one.
// ---------------------------------------------------------------------------------------------

/// Open the pipe, write `frame(req.encode())`, read one framed `Response`. `None` on any failure.
fn rpc(req: Request) -> Option<Response> {
    let pipe = PIPE.get()?;
    let h = open_pipe(pipe)?;
    let resp = (|| {
        let body = proto::frame(&req.encode());
        write_all(h, &body)?;
        read_response(h)
    })();
    // SAFETY: `h` is a valid handle returned by CreateFileW and not used after this call.
    unsafe {
        let _ = CloseHandle(h);
    }
    resp
}

/// `CreateFileW` the named pipe for read+write. Returns `None` if it cannot be opened.
fn open_pipe(path: &str) -> Option<HANDLE> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call; all other args are
    // plain values. CreateFileW returns a Result (Err on INVALID_HANDLE_VALUE), which we map to None.
    unsafe {
        CreateFileW(
            PCWSTR::from_raw(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
        .ok()
    }
}

/// Write the whole buffer, looping over partial writes. `Some(())` on success.
fn write_all(h: HANDLE, buf: &[u8]) -> Option<()> {
    let mut off = 0usize;
    while off < buf.len() {
        let mut written: u32 = 0;
        // SAFETY: `h` is valid; the slice is in-bounds; `written` is a live out-param.
        unsafe {
            WriteFile(h, Some(&buf[off..]), Some(&mut written), None).ok()?;
        }
        if written == 0 {
            return None; // pipe closed mid-write
        }
        off += written as usize;
    }
    Some(())
}

/// Read a 4-byte LE length prefix, then that many bytes, then `Response::decode`.
fn read_response(h: HANDLE) -> Option<Response> {
    let mut len_buf = [0u8; 4];
    read_exact(h, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > proto::MAX_TEXT_BYTES + 16 {
        return None; // refuse absurd frames (matches proto::parse_frame's cap)
    }
    let mut body = vec![0u8; len];
    read_exact(h, &mut body)?;
    Response::decode(&body).ok()
}

/// Loop `ReadFile` until `buf` is full. `None` on short read (EOF) or error.
fn read_exact(h: HANDLE, buf: &mut [u8]) -> Option<()> {
    let mut off = 0usize;
    while off < buf.len() {
        let mut read: u32 = 0;
        // SAFETY: `h` is valid; the destination slice is in-bounds; `read` is a live out-param.
        unsafe {
            ReadFile(h, Some(&mut buf[off..]), Some(&mut read), None).ok()?;
        }
        if read == 0 {
            return None; // EOF before the buffer was filled
        }
        off += read as usize;
    }
    Some(())
}

// ---------------------------------------------------------------------------------------------
// Host-store view used by the detours. Each is fail-soft (None / no-op on a down pipe).
// ---------------------------------------------------------------------------------------------

pub(crate) fn store_get() -> Option<String> {
    match rpc(Request::Get)? {
        Response::Text(t) => t,
        _ => None,
    }
}

pub(crate) fn store_set(s: &str) {
    let _ = rpc(Request::Set(s.to_string()));
}

pub(crate) fn store_empty() {
    let _ = rpc(Request::Empty);
}

pub(crate) fn store_seq() -> u32 {
    match rpc(Request::Seq) {
        Some(Response::Seq(n)) => n as u32,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// HGLOBAL <-> text helpers used by SetClipboardData / GetClipboardData detours.
// ---------------------------------------------------------------------------------------------

/// Read a UTF-16 string out of an app-provided `HGLOBAL` (as handed to `SetClipboardData`).
/// Reads up to the first NUL (or the whole block if unterminated). `None` if lock fails / empty.
pub(crate) fn read_utf16_from_hglobal(h: HGLOBAL) -> Option<String> {
    // SAFETY: `h` is the handle the app passed to SetClipboardData; GlobalLock pins it and returns
    // a pointer valid until GlobalUnlock. GlobalSize gives the block's byte size so we never read
    // out of bounds. We treat the bytes as UTF-16 code units (CF_UNICODETEXT contract).
    unsafe {
        let ptr = GlobalLock(h) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let bytes = GlobalSize(h);
        let units = bytes / 2;
        let slice = std::slice::from_raw_parts(ptr, units);
        // Stop at the first NUL terminator if present.
        let end = slice.iter().position(|&c| c == 0).unwrap_or(units);
        let text = String::from_utf16_lossy(&slice[..end]);
        let _ = GlobalUnlock(h);
        Some(text)
    }
}

/// Allocate a `GMEM_MOVEABLE` `HGLOBAL` and write `text` into it in the encoding `fmt` requires:
/// UTF-16 (+ NUL) for `CF_UNICODETEXT`, single-byte (lossy) for `CF_TEXT`/`CF_OEMTEXT`. Returns
/// the handle the caller must cache + eventually free; `None` on allocation failure.
pub(crate) fn alloc_hglobal_for(fmt: u32, text: &str) -> Option<HGLOBAL> {
    if fmt == CF_UNICODETEXT_ID {
        let mut units: Vec<u16> = text.encode_utf16().collect();
        units.push(0); // NUL terminator
        let bytes = units.len() * 2;
        // SAFETY: GlobalAlloc(GMEM_MOVEABLE) returns a movable handle; GlobalLock pins it to a
        // writable pointer valid for `bytes`. We copy exactly `units.len()` u16s (== `bytes`) then
        // unlock. On any failure we return None without leaking (alloc failed ⇒ nothing to free).
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let dst = GlobalLock(h) as *mut u16;
            if dst.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
            let _ = GlobalUnlock(h);
            Some(h)
        }
    } else {
        // CF_TEXT / CF_OEMTEXT: down-convert to single-byte. Non-ASCII becomes '?' (lossy, v1).
        let mut bytes: Vec<u8> = text
            .chars()
            .map(|c| if (c as u32) < 0x80 { c as u8 } else { b'?' })
            .collect();
        bytes.push(0); // NUL terminator
        let n = bytes.len();
        // SAFETY: as above, sized to `n` bytes; we copy exactly `n` bytes then unlock.
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, n).ok()?;
            let dst = GlobalLock(h) as *mut u8;
            if dst.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
            let _ = GlobalUnlock(h);
            Some(h)
        }
    }
}

/// Resolve a user32 export's absolute address. Using an absolute address (rather than naming the
/// import) detours *all* call sites in the process, not just this DLL's IAT slot.
///
/// # Safety
/// Caller transmutes the returned address to the export's exact ABI signature.
pub(crate) unsafe fn user32_proc(name: &[u8]) -> Option<*const ()> {
    // `name` must be a NUL-terminated ASCII byte string (e.g. b"OpenClipboard\0").
    // SAFETY: "user32.dll\0" is a valid module name literal; GetModuleHandleW returns the loaded
    // module (user32 is always present in a GUI process). GetProcAddress returns the export or None.
    let module = GetModuleHandleW(PCWSTR::from_raw(USER32_W.as_ptr())).ok()?;
    let proc = GetProcAddress(module, PCSTR::from_raw(name.as_ptr()));
    proc.map(|f| f as *const ())
}

/// `"user32.dll\0"` as UTF-16 (module name for `GetModuleHandleW`).
static USER32_W: [u16; 11] = [
    b'u' as u16,
    b's' as u16,
    b'e' as u16,
    b'r' as u16,
    b'3' as u16,
    b'2' as u16,
    b'.' as u16,
    b'd' as u16,
    b'l' as u16,
    b'l' as u16,
    0,
];
