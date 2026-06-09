//! user32-only clipboard probe for on-box validation of the private-clipboard hook.
//!
//! `clipprobe roundtrip <text>` does a plain Win32 `SetClipboardData(CF_UNICODETEXT, <text>)`
//! followed by `GetClipboardData(CF_UNICODETEXT)`, printing `READBACK=<value>`.
//!
//! Run UNBOXED it hits the real clipboard. Run inside a glass Sandboxie box (hook injected, the
//! box also carrying `OpenClipboard=n`) the *only* way these calls can succeed is if the injected
//! hook intercepts them and serves the private store — so a correct `READBACK` proves interception.
//! Deliberately uses the raw user32 path (not OLE / .NET `Clipboard`), which is exactly what the
//! v1 hook detours. Windows-only; a no-op elsewhere so the Linux dev box stays green.
//!   cargo build -p glass-clip-hook --release --example clipprobe

#[cfg(windows)]
fn main() {
    std::process::exit(run());
}

#[cfg(windows)]
fn run() -> i32 {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("roundtrip") {
        eprintln!("usage: clipprobe roundtrip <text>");
        return 2;
    }
    let text = args.get(2).cloned().unwrap_or_default();
    let cf = CF_UNICODETEXT.0 as u32;
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = std::mem::size_of_val(&utf16[..]);

    unsafe {
        // --- write ---
        // SAFETY: standard user32 clipboard write of a CF_UNICODETEXT GMEM_MOVEABLE block; each
        // call is checked and the clipboard is closed before returning. On success the system (or
        // the hook) owns the handle, so we do not free it.
        if OpenClipboard(None).is_err() {
            eprintln!("FAIL: OpenClipboard(write)");
            return 1;
        }
        let _ = EmptyClipboard();
        let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            let _ = CloseClipboard();
            eprintln!("FAIL: GlobalAlloc");
            return 1;
        };
        let dst = GlobalLock(h);
        if dst.is_null() {
            let _ = CloseClipboard();
            eprintln!("FAIL: GlobalLock(write)");
            return 1;
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr() as *const u8, dst as *mut u8, bytes);
        let _ = GlobalUnlock(h);
        if SetClipboardData(cf, HANDLE(h.0)).is_err() {
            let _ = CloseClipboard();
            eprintln!("FAIL: SetClipboardData");
            return 1;
        }
        let _ = CloseClipboard();

        // --- read back ---
        // SAFETY: standard user32 clipboard read; the returned handle is owned by the clipboard
        // (we must not free it); we lock/copy/unlock and close.
        if OpenClipboard(None).is_err() {
            eprintln!("FAIL: OpenClipboard(read)");
            return 1;
        }
        let read = match GetClipboardData(cf) {
            Ok(hr) if !hr.is_invalid() => {
                let g = HGLOBAL(hr.0);
                let p = GlobalLock(g) as *const u16;
                if p.is_null() {
                    String::new()
                } else {
                    let mut len = 0usize;
                    while *p.add(len) != 0 {
                        len += 1;
                    }
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                    let _ = GlobalUnlock(g);
                    s
                }
            }
            _ => String::new(),
        };
        let _ = CloseClipboard();

        println!("READBACK={read}");
    }
    0
}

#[cfg(not(windows))]
fn main() {}
