//! Host-side named-pipe server backing a boxed app's private clipboard.
//!
//! One server per contained app: a thread runs an accept loop on `\\.\pipe\glass-clip-<box>`
//! with a security descriptor scoped to the current user (the box runs at user integrity under
//! `KeepTokenIntegrity=y`). Each connection carries one length-prefixed [`proto`] request, which
//! we [`PrivateClipboard::apply`] and answer. The boxed `glass_clip_hook` DLL is the client.
//! Sandboxie's `OpenPipePath` lets the box reach this host pipe (always applied).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use glass_clip_hook::proto::{self, Request, Response};
use glass_clip_hook::store::PrivateClipboard;
use glass_core::{GlassError, Result};

use windows::core::BOOL;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// SDDL for the pipe DACL: Everyone (WD) + SYSTEM generic-all. Everyone is required merely so the
/// box's client can *connect*: with `KeepTokenIntegrity=y` the Sandboxie token's access check is
/// satisfied only by a SID in its restricting set, so an owner-only DACL is denied. The DACL is
/// therefore NOT the authorization boundary — every connection is gated by [`client_in_box`], which
/// verifies the client PID belongs to THIS Sandboxie box (`Start.exe /listpids`) before a request is
/// honored. So an ordinary local process that guessed the per-box pipe name is rejected.
const PIPE_SDDL: &str = "D:(A;;GA;;;WD)(A;;GA;;;SY)";

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A running clipboard pipe server. Dropping/`stop`ping it ends the accept loop and joins.
pub(crate) struct ClipServer {
    pipe_path: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ClipServer {
    /// Start the accept loop for `pipe_name` (e.g. `glass-clip-glass_1234`), serving `store`. `dir`
    /// + `box_name` identify the Sandboxie box so each connection can be gated to its members.
    pub(crate) fn start(
        pipe_name: &str,
        store: PrivateClipboard,
        dir: String,
        box_name: String,
    ) -> Result<ClipServer> {
        let pipe_path = format!(r"\\.\pipe\{pipe_name}");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let path_t = pipe_path.clone();
        let thread = std::thread::Builder::new()
            .name(format!("glass-clip:{pipe_name}"))
            .spawn(move || accept_loop(&path_t, store, stop_t, dir, box_name))
            .map_err(|e| GlassError::Backend(format!("spawn clip server: {e}")))?;
        Ok(ClipServer {
            pipe_path,
            stop,
            thread: Some(thread),
        })
    }

    /// Stop the loop and join its accept thread. Thin consuming wrapper over
    /// [`Self::shutdown`]; `Drop` also calls `shutdown`, so a `ClipServer`
    /// dropped without an explicit `stop()` is reclaimed just the same.
    pub(crate) fn stop(mut self) {
        self.shutdown();
    }

    /// Flag the loop, poke the pipe (a self-connect) to break a blocking
    /// `ConnectNamedPipe`, then join the accept thread. Idempotent: the thread
    /// handle is taken via the `Option`, so calling this twice (e.g. `stop()`
    /// then `Drop`) joins exactly once.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let wide = to_wide(&self.pipe_path);
        // SAFETY: opening our own pipe by name to release the accept loop; handle closed immediately.
        unsafe {
            use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
            use windows::Win32::Storage::FileSystem::{
                CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
            };
            if let Ok(h) = CreateFileW(
                PCWSTR(wide.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            ) {
                let _ = CloseHandle(h);
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ClipServer {
    fn drop(&mut self) {
        // Full teardown even when stop() was never called — e.g. an early
        // return in setup_private_clipboard, or a launch() that fails after the
        // server started — so the accept thread + pipe instance + security
        // descriptor are never leaked (previously Drop only set the flag, which
        // a thread parked in ConnectNamedPipe never observes).
        self.shutdown();
    }
}

/// Build a SECURITY_ATTRIBUTES from `PIPE_SDDL`. Returns the descriptor alongside so its memory
/// outlives the SA (the SA holds a raw pointer into it).
fn make_security() -> Result<(SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR)> {
    let wsddl = to_wide(PIPE_SDDL);
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: wsddl is a NUL-terminated SDDL string; the call allocates a self-relative SD into
    // psd which we free when the server stops (LocalFree). Single-threaded setup, no aliasing.
    unsafe {
        use windows::Win32::Security::Authorization::SDDL_REVISION_1;
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wsddl.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .map_err(|e| GlassError::Backend(format!("clip pipe SDDL: {e}")))?;
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: BOOL(0),
    };
    Ok((sa, psd))
}

fn accept_loop(
    pipe_path: &str,
    store: PrivateClipboard,
    stop: Arc<AtomicBool>,
    dir: String,
    box_name: String,
) {
    let wide = to_wide(pipe_path);
    let (sa, psd) = match make_security() {
        Ok(v) => v,
        Err(_) => return, // server unavailable → Layer-1-only (user still protected)
    };
    // Cache the box's PID set briefly: the gate needs it per connection, but `Start.exe /listpids`
    // is a subprocess, so refresh at most ~twice a second (clipboard ops are rare; the box's pids
    // are stable within a session).
    let mut pids: (Instant, Vec<u32>) = (Instant::now(), Vec::new());
    while !stop.load(Ordering::Relaxed) {
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        // SAFETY: classic CreateNamedPipeW; `wide` is NUL-terminated; sa points at a live SD.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                4,
                64 * 1024,
                64 * 1024,
                0,
                Some(&sa),
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            break;
        }
        // SAFETY: blocks until a client connects (or our self-poke in stop()).
        // A client that connects in the gap between CreateNamedPipeW and
        // ConnectNamedPipe makes the call return FALSE + ERROR_PIPE_CONNECTED —
        // which IS a connected client. windows-rs maps that FALSE to Err, so
        // `.is_ok()` alone would silently drop the client (its read-back then
        // comes back empty — the flaky-empty-readback class). Treat it as
        // connected.
        let connected = match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => true,
            Err(e) => e.code() == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0),
        };
        if stop.load(Ordering::Relaxed) {
            // SAFETY: closing the instance handle we own.
            unsafe {
                let _ = DisconnectNamedPipe(pipe);
                let _ = CloseHandle(pipe);
            }
            break;
        }
        if connected {
            if pids.1.is_empty() || pids.0.elapsed() > Duration::from_millis(500) {
                pids = (Instant::now(), box_pids(&dir, &box_name));
            }
            serve_one(pipe, &store, &pids.1);
        }
        // SAFETY: done with this instance.
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
        }
    }
    // SAFETY: free the SD allocated by ConvertStringSecurityDescriptor…; psd.0 is the pointer
    // returned by that API via LocalAlloc and must be freed with LocalFree.
    unsafe {
        use windows::Win32::Foundation::HLOCAL;
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(psd.0)));
    }
}

/// The box's current PID set via `Start.exe /box:<box> /listpids` — the authoritative Sandboxie
/// membership list (the same call the backend uses for discovery). Empty on any failure, so the
/// gate fails closed.
fn box_pids(dir: &str, box_name: &str) -> Vec<u32> {
    match std::process::Command::new(format!(r"{dir}\Start.exe"))
        .args([&format!("/box:{box_name}"), "/listpids"])
        .output()
    {
        Ok(out) => super::config::parse_listpids(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => Vec::new(),
    }
}

/// Authorization gate: is the connected pipe client a process inside THIS Sandboxie box?
///
/// The broad pipe DACL only lets the box connect at all (its `KeepTokenIntegrity` token fails an
/// owner-only DACL); this membership check is the real boundary. We look up the client PID
/// (`GetNamedPipeClientProcessId`) and require it to be in the box's `/listpids` set — so an
/// ordinary local process that guessed the per-box pipe name is rejected. Fail closed on any error.
fn client_in_box(pipe: HANDLE, box_pids: &[u32]) -> bool {
    let mut pid = 0u32;
    // SAFETY: writes the connected client's PID into `pid`; returns Err if there is no client.
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.is_err() || pid == 0 {
        return false;
    }
    box_pids.contains(&pid)
}

/// Read one length-prefixed request, apply it, write the length-prefixed response.
fn serve_one(pipe: HANDLE, store: &PrivateClipboard, box_pids: &[u32]) {
    let Some(body) = read_frame(pipe) else { return };
    // Authorization: only a process inside our Sandboxie box is served — the DACL merely lets the
    // box connect; this is the boundary against any other local process (see [`client_in_box`]).
    if !client_in_box(pipe, box_pids) {
        return;
    }
    let resp = match Request::decode(&body) {
        Ok(req) => store.apply(req),
        Err(_) => Response::Bytes(None), // skew/garbage → benign empty, never a crash
    };
    let out = proto::frame(&resp.encode());
    let mut written = 0u32;
    // SAFETY: write the response, then FlushFileBuffers — it blocks until the client has read all
    // the bytes. Without it, the caller's DisconnectNamedPipe can fire first and DISCARD the unread
    // response (the read-back then comes back empty), which is timing-dependent and flaky.
    unsafe {
        let _ = WriteFile(pipe, Some(&out), Some(&mut written as *mut u32), None);
        let _ = FlushFileBuffers(pipe);
    }
}

/// Read a 4-byte LE length then exactly that many bytes. None on any short/failed read.
fn read_frame(pipe: HANDLE) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    if !read_exact(pipe, &mut len_buf) {
        return None;
    }
    let n = u32::from_le_bytes(len_buf) as usize;
    if n > proto::MAX_TOTAL_BYTES + 4096 {
        return None;
    }
    let mut body = vec![0u8; n];
    if !read_exact(pipe, &mut body) {
        return None;
    }
    Some(body)
}

fn read_exact(pipe: HANDLE, buf: &mut [u8]) -> bool {
    let mut off = 0usize;
    while off < buf.len() {
        let mut read = 0u32;
        // SAFETY: reading into buf[off..] on a connected pipe instance we own.
        let ok = unsafe {
            ReadFile(
                pipe,
                Some(&mut buf[off..]),
                Some(&mut read as *mut u32),
                None,
            )
        }
        .is_ok();
        if !ok || read == 0 {
            return false;
        }
        off += read as usize;
    }
    true
}
