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
    let mode = std::env::args().nth(1).unwrap_or_default();
    let code = match mode.as_str() {
        "roundtrip" => run(),
        "roundtrip-multi" => run_multi(),
        _ => {
            eprintln!("usage: clipprobe <roundtrip|roundtrip-multi> [text]");
            2
        }
    };
    std::process::exit(code);
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

/// Allocate a `GMEM_MOVEABLE` `HGLOBAL` holding `data`; null handle on failure.
///
/// # Safety
/// Standard `GlobalAlloc`/`GlobalLock`/copy/`GlobalUnlock`. The returned handle is handed to
/// `SetClipboardData` (which takes ownership on success); the caller must not free it.
#[cfg(windows)]
unsafe fn alloc_global(data: &[u8]) -> windows::Win32::Foundation::HGLOBAL {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, data.len()) else {
        return HGLOBAL(std::ptr::null_mut());
    };
    let dst = GlobalLock(h);
    if dst.is_null() {
        return HGLOBAL(std::ptr::null_mut());
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, data.len());
    let _ = GlobalUnlock(h);
    h
}

/// `GetClipboardData(fmt)` → the full `HGLOBAL` contents as bytes (bounded by `GlobalSize`). `None`
/// if the format is absent or the handle is not a lockable HGLOBAL (e.g. a GDI `CF_BITMAP`).
///
/// # Safety
/// The returned handle is owned by the clipboard; we lock/copy/unlock without freeing it.
#[cfg(windows)]
unsafe fn read_global_bytes(fmt: u32) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    let hr = GetClipboardData(fmt).ok()?;
    if hr.is_invalid() {
        return None;
    }
    let g = HGLOBAL(hr.0);
    let p = GlobalLock(g) as *const u8;
    if p.is_null() {
        return None;
    }
    let n = GlobalSize(g);
    let v = std::slice::from_raw_parts(p, n).to_vec();
    let _ = GlobalUnlock(g);
    Some(v)
}

/// Multi-format probe: write `CF_UNICODETEXT` + the registered `"HTML Format"` (round-tripped by
/// NAME) + a tiny valid `CF_DIB` in ONE clipboard session, then read each back — plus `CF_BITMAP`,
/// which the hook must SYNTHESIZE from the stored DIB via GDI. Prints one `READBACK-*` line per
/// format and a final `PROBE-MULTI-DONE`. Boxed (hook + `OpenClipboard=n`), every line proves the
/// hook captured/served/synthesized that format from the private store.
#[cfg(windows)]
fn run_multi() -> i32 {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
        SetClipboardData,
    };
    use windows::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_UNICODETEXT};

    // Three payloads.
    let utf16: Vec<u16> = "FROM-BOX-MULTI".encode_utf16().chain(std::iter::once(0)).collect();
    let text_bytes: Vec<u8> = utf16.iter().flat_map(|u| u.to_le_bytes()).collect();
    let html_bytes = b"<b>hi</b>".to_vec();
    // A 2x2 32bpp BI_RGB DIB: 40-byte BITMAPINFOHEADER + 16 pixel bytes = 56 bytes.
    let mut dib: Vec<u8> = Vec::with_capacity(56);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&2i32.to_le_bytes()); // biWidth
    dib.extend_from_slice(&2i32.to_le_bytes()); // biHeight
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // xppm
    dib.extend_from_slice(&0i32.to_le_bytes()); // yppm
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    dib.extend_from_slice(&[0u8; 16]); // 2x2 * 4 bytes

    let cf_text = CF_UNICODETEXT.0 as u32;
    let cf_dib = CF_DIB.0 as u32;
    let cf_bmp = CF_BITMAP.0 as u32;

    unsafe {
        let html_w: Vec<u16> = "HTML Format".encode_utf16().chain(std::iter::once(0)).collect();
        let html_fmt = RegisterClipboardFormatW(PCWSTR::from_raw(html_w.as_ptr()));
        if html_fmt == 0 {
            eprintln!("FAIL: RegisterClipboardFormatW");
            return 1;
        }

        // --- write all three in one session ---
        if OpenClipboard(None).is_err() {
            eprintln!("FAIL: OpenClipboard(write)");
            return 1;
        }
        let _ = EmptyClipboard();
        for (fmt, data) in [(cf_text, &text_bytes), (html_fmt, &html_bytes), (cf_dib, &dib)] {
            let h = alloc_global(data);
            if h.0.is_null() {
                let _ = CloseClipboard();
                eprintln!("FAIL: alloc {fmt}");
                return 1;
            }
            if SetClipboardData(fmt, HANDLE(h.0)).is_err() {
                let _ = CloseClipboard();
                eprintln!("FAIL: SetClipboardData {fmt}");
                return 1;
            }
        }
        let _ = CloseClipboard();

        // --- read back ---
        if OpenClipboard(None).is_err() {
            eprintln!("FAIL: OpenClipboard(read)");
            return 1;
        }
        let text_rb = read_global_bytes(cf_text)
            .map(|b| {
                let units: Vec<u16> =
                    b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
                String::from_utf16_lossy(&units[..len])
            })
            .unwrap_or_default();
        let html_rb = read_global_bytes(html_fmt)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let dib_len = read_global_bytes(cf_dib).map(|b| b.len()).unwrap_or(0);
        // CF_BITMAP is a GDI HBITMAP (NOT an HGLOBAL) synthesized by the hook from the stored DIB —
        // a valid handle proves the GDI synthesis path works on-box.
        let bmp_ok = matches!(GetClipboardData(cf_bmp), Ok(hr) if !hr.is_invalid());
        let _ = CloseClipboard();

        println!("READBACK-TEXT={text_rb}");
        println!("READBACK-HTML={html_rb}");
        println!("READBACK-DIB-LEN={dib_len}");
        println!("READBACK-BMP={}", if bmp_ok { "OK" } else { "NULL" });
        println!("PROBE-MULTI-DONE");
    }
    0
}

#[cfg(not(windows))]
fn main() {}
