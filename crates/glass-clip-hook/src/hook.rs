//! user32 clipboard detours + the pipe client (cfg windows; runtime-validated on LOTUS).
//!
//! Emulates a per-app clipboard backed by the host store. The detours NEVER call the real
//! clipboard APIs (the box also has `OpenClipboard=n`), so the user's clipboard is untouched.
//! v2: arbitrary `format → bytes` items captured into a pending set and committed atomically on
//! `CloseClipboard`; on read we serve any stored format verbatim, or synthesize a derivative from
//! its canonical stored form (text triad/LOCALE ← CF_UNICODETEXT; CF_BITMAP/DIBV5 ← CF_DIB). Eager
//! data only — delayed rendering (`SetClipboardData(_, NULL)`) and OLE are out of scope.
//!
//! Compile-time verified by cross-compiling to `x86_64-pc-windows-gnu`. Runtime interception is
//! finalized on a real Windows box (LOTUS); the detour table is authored complete so that on-box
//! work is debugging, not authoring.

use std::sync::OnceLock;

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GlobalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, HGLOBAL,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use windows::Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW};

use crate::proto::{self, FormatKey, Request, Response};

mod detour_impl;
mod dataobject;
mod ole;

// Standard Win32 clipboard format ids (stable ABI).
pub(crate) const CF_TEXT: u32 = 1;
pub(crate) const CF_BITMAP: u32 = 2;
pub(crate) const CF_OEMTEXT: u32 = 7;
// CF_DIB and CF_UNICODETEXT are referenced by the OLE serve path (Tasks 3/4) but not yet used;
// suppress dead-code warnings until those modules are filled in.
#[allow(dead_code)]
pub(crate) const CF_DIB: u32 = 8;
#[allow(dead_code)]
pub(crate) const CF_UNICODETEXT: u32 = 13;
pub(crate) const CF_LOCALE: u32 = 16;
pub(crate) const CF_DIBV5: u32 = 17;

/// Raw clipboard id → `FormatKey`: a registered (>=0xC000) id resolves to its NAME (portable across
/// processes), everything else stays a `Standard` id.
pub(crate) fn key_of(id: u32) -> FormatKey {
    if id >= 0xC000 {
        let mut buf = [0u16; 256];
        // SAFETY: GetClipboardFormatNameW writes up to buf.len() chars and returns the count (0 on
        // failure); the slice bounds the read.
        let n = unsafe { GetClipboardFormatNameW(id, &mut buf) };
        if n > 0 {
            return FormatKey::Named(String::from_utf16_lossy(&buf[..n as usize]));
        }
    }
    FormatKey::Standard(id)
}

/// `FormatKey` → this process's clipboard id (re-registering named formats so the id is valid here).
pub(crate) fn id_of(key: &FormatKey) -> u32 {
    match key {
        FormatKey::Standard(id) => *id,
        FormatKey::Named(name) => {
            let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `w` is NUL-terminated and outlives the call; RegisterClipboardFormatW returns
            // this session's id for that name.
            unsafe { RegisterClipboardFormatW(windows::core::PCWSTR::from_raw(w.as_ptr())) }
        }
    }
}

/// 4-byte `CF_LOCALE` blob = the current input-language LCID.
pub(crate) fn locale_blob() -> Vec<u8> {
    use windows::Win32::Globalization::GetUserDefaultLCID;
    // SAFETY: GetUserDefaultLCID takes no args and returns the LCID.
    let lcid = unsafe { GetUserDefaultLCID() };
    lcid.to_le_bytes().to_vec()
}

/// Resolved pipe path (`\\.\pipe\<GLASS_CLIP_PIPE>`). `None` ⇒ the DLL is inert.
static PIPE: OnceLock<String> = OnceLock::new();

/// Called from `InjectDllMain`. Inert if `GLASS_CLIP_PIPE` is unset (only the target app's
/// process tree carries it; every other boxed process loads this DLL but stays dormant).
pub(crate) fn init() {
    let Ok(name) = std::env::var("GLASS_CLIP_PIPE") else {
        return;
    };
    if name.is_empty() {
        return;
    }
    // Only install detours when we newly set the pipe — a second `init()` (re-injection) must not
    // re-patch already-enabled hooks.
    if PIPE.set(format!(r"\\.\pipe\{name}")).is_ok() {
        detour_impl::install();
    }
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

/// `CreateFileW` the named pipe for read+write, retrying briefly. Returns `None` if it cannot be
/// opened within the retry budget.
///
/// The host runs a single-instance accept loop: after it serves one connection there is a short gap
/// before it re-creates the next pipe instance. A `Set` immediately followed by a `Get` (as a
/// clipboard write→read does) races into that gap, so a single `CreateFile` would intermittently
/// fail. We use the standard named-pipe client pattern: on failure, `WaitNamedPipe` for an instance
/// (and back off briefly if the pipe is momentarily absent), then retry — bounded so a genuinely
/// down server still fails soft rather than hanging.
fn open_pipe(path: &str) -> Option<HANDLE> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    for _ in 0..40 {
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call; all other args
        // are plain values. CreateFileW returns Err on INVALID_HANDLE_VALUE, which we map to None.
        let opened = unsafe {
            CreateFileW(
                PCWSTR::from_raw(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        if let Ok(h) = opened {
            return Some(h);
        }
        // Instance busy → WaitNamedPipe blocks until one frees (up to 100ms). Instance momentarily
        // absent → it returns Err fast, so back off ~10ms to avoid a tight spin, then retry.
        // SAFETY: `wide` outlives the call; WaitNamedPipeW is a simple blocking wait by name.
        let waited = unsafe { WaitNamedPipeW(PCWSTR::from_raw(wide.as_ptr()), 100) }.as_bool();
        if !waited {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    None
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
    if len > proto::MAX_TOTAL_BYTES + 4096 {
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

/// Ship one atomic multi-format copy to the host.
pub(crate) fn store_set_all(items: Vec<(FormatKey, Vec<u8>)>) {
    let _ = rpc(Request::SetAll(items));
}

/// The host's stored format keys (canonical only; synthesis is computed locally via `synth`).
pub(crate) fn store_list() -> Vec<FormatKey> {
    match rpc(Request::List) {
        Some(Response::Formats(k)) => k,
        _ => Vec::new(),
    }
}

/// One stored format's bytes from the host (no synthesis).
pub(crate) fn store_get_bytes(key: &FormatKey) -> Option<Vec<u8>> {
    match rpc(Request::Get(key.clone()))? {
        Response::Bytes(b) => b,
        _ => None,
    }
}

pub(crate) fn store_empty() {
    let _ = rpc(Request::Empty);
}

pub(crate) fn store_seq() -> u32 {
    match rpc(Request::Seq) {
        // u64→u32 truncation is fine: this feeds Win32 GetClipboardSequenceNumber (also u32) and
        // only drives change-detection, so a wrap every 4 billion sets is harmless.
        Some(Response::Seq(n)) => n as u32,
        _ => 0,
    }
}

/// The whole stored snapshot (for the OLE serve path's one-shot bake).
// Used by the OLE dataobject proxy (Task 3); suppress dead-code until that module is filled in.
#[allow(dead_code)]
pub(crate) fn store_get_all() -> Vec<(FormatKey, Vec<u8>)> {
    match rpc(Request::GetAll) {
        Some(Response::Items(items)) => items,
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Generic HGLOBAL byte helpers + the one text code-page conversion, used by the detours.
// ---------------------------------------------------------------------------------------------

/// Copy the full contents of an app-provided `HGLOBAL` to a `Vec<u8>` (bounded by `GlobalSize`).
pub(crate) fn read_bytes_from_hglobal(h: HGLOBAL) -> Option<Vec<u8>> {
    // SAFETY: GlobalLock pins `h`; GlobalSize bounds the slice so we never read OOB; slice→Vec is safe.
    unsafe {
        let ptr = GlobalLock(h) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let n = GlobalSize(h);
        let v = std::slice::from_raw_parts(ptr, n).to_vec();
        let _ = GlobalUnlock(h);
        Some(v)
    }
}

/// Allocate a `GMEM_MOVEABLE` `HGLOBAL` holding exactly `bytes`. Caller caches + frees it.
pub(crate) fn alloc_hglobal_bytes(bytes: &[u8]) -> Option<HGLOBAL> {
    // SAFETY: GlobalAlloc(GMEM_MOVEABLE) then GlobalLock to a writable ptr valid for bytes.len();
    // copy exactly bytes.len(); unlock. Free on lock failure to avoid a leak.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
        let dst = GlobalLock(h) as *mut u8;
        if dst.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

/// CF_UNICODETEXT bytes → CF_TEXT/CF_OEMTEXT bytes via the real code page (ANSI/OEM).
pub(crate) fn unicode_to_codepage(utf16_bytes: &[u8], oem: bool) -> Vec<u8> {
    use windows::Win32::Globalization::{WideCharToMultiByte, CP_ACP, CP_OEMCP};
    let units: Vec<u16> = utf16_bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let cp = if oem { CP_OEMCP } else { CP_ACP };
    // SAFETY: two-call WideCharToMultiByte (size, then fill); `units` outlives both; output sized exactly.
    unsafe {
        let len = WideCharToMultiByte(cp, 0, &units, None, PCSTR::null(), None);
        if len <= 0 {
            return vec![0];
        }
        let mut out = vec![0u8; len as usize];
        WideCharToMultiByte(cp, 0, &units, Some(&mut out), PCSTR::null(), None);
        out
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

/// Resolve an `ole32` export's absolute address (so all call sites are detoured). See `user32_proc`.
///
/// # Safety
/// Caller transmutes the returned address to the export's exact ABI signature.
// Used by the OLE detours (Task 4); suppress dead-code until that module is filled in.
#[allow(dead_code)]
pub(crate) unsafe fn ole32_proc(name: &[u8]) -> Option<*const ()> {
    // SAFETY: "ole32.dll\0" is a valid module name; an OLE-using app has it loaded. We do NOT
    // LoadLibrary it — if it isn't loaded, the app isn't using OLE and there's nothing to hook.
    let module = GetModuleHandleW(PCWSTR::from_raw(OLE32_W.as_ptr())).ok()?;
    let proc = GetProcAddress(module, PCSTR::from_raw(name.as_ptr()));
    proc.map(|f| f as *const ())
}

/// `"ole32.dll\0"` as UTF-16.
#[allow(dead_code)]
static OLE32_W: [u16; 10] = [
    b'o' as u16, b'l' as u16, b'e' as u16, b'3' as u16, b'2' as u16, b'.' as u16, b'd' as u16,
    b'l' as u16, b'l' as u16, 0,
];
