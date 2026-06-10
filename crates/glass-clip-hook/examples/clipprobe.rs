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
        "roundtrip-ole" => run_ole(),
        "roundtrip-hdrop" => run_hdrop(),
        _ => {
            eprintln!(
                "usage: clipprobe <roundtrip|roundtrip-multi|roundtrip-ole|roundtrip-hdrop> [text]"
            );
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
        if SetClipboardData(cf, Some(HANDLE(h.0))).is_err() {
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
            if SetClipboardData(fmt, Some(HANDLE(h.0))).is_err() {
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

// ---------------------------------------------------------------------------
// OLE round-trip probe (2a-ii).
//
// `clipprobe roundtrip-ole` exercises the OLE clipboard surface (`OleSetClipboard` /
// `OleGetClipboard`), which the hook detours separately from the user32 path. It builds a tiny
// in-process `IDataObject` offering CF_UNICODETEXT + a named "HTML Format" over `TYMED_HGLOBAL`,
// pushes it with `OleSetClipboard` (→ glass marshals it into the private store), then reads it back
// two ways: via `OleGetClipboard` (→ glass's proxy IDataObject) AND via raw user32
// `GetClipboardData` (cross-surface coherence — the OLE copy must also be visible to the user32
// serve path). Prints `OLE-TEXT=` / `OLE-HTML=` / `U32-TEXT=` and a final `PROBE-OLE-DONE`.
// ---------------------------------------------------------------------------

/// Run a vtable-method body, converting a panic (UB across the `extern "system"` COM boundary) into
/// an error HRESULT. The `#[implement]` thunks have no panic guard, so every `_Impl` method guards
/// itself (mirrors `hook/dataobject.rs`).
#[cfg(windows)]
fn guard_hr(f: impl FnOnce() -> windows::core::HRESULT) -> windows::core::HRESULT {
    use windows::Win32::Foundation::E_UNEXPECTED;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(E_UNEXPECTED)
}

#[cfg(windows)]
fn guard_res<T>(f: impl FnOnce() -> windows::core::Result<T>) -> windows::core::Result<T> {
    use windows::Win32::Foundation::E_UNEXPECTED;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(E_UNEXPECTED.into()))
}

/// Build a fresh HGLOBAL/content `FORMATETC` for `cf` (rebuilt per id, never stored/cloned).
#[cfg(windows)]
fn probe_formatetc(cf: u16) -> windows::Win32::System::Com::FORMATETC {
    use windows::Win32::System::Com::{FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL};
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Enumerator over the probe object's two format ids — `AtomicUsize` cursor, `FORMATETC` built on
/// demand (mirrors `StoreEnum` in `hook/dataobject.rs`).
#[cfg(windows)]
#[windows::core::implement(windows::Win32::System::Com::IEnumFORMATETC)]
struct ProbeEnum {
    ids: Vec<u16>,
    idx: std::sync::atomic::AtomicUsize,
}

#[cfg(windows)]
#[allow(non_snake_case)]
impl windows::Win32::System::Com::IEnumFORMATETC_Impl for ProbeEnum_Impl {
    fn Next(
        &self,
        celt: u32,
        rgelt: *mut windows::Win32::System::Com::FORMATETC,
        pceltfetched: *mut u32,
    ) -> windows::core::HRESULT {
        use std::sync::atomic::Ordering;
        use windows::Win32::Foundation::{S_FALSE, S_OK};
        guard_hr(|| {
            let mut n = 0u32;
            let mut i = self.idx.load(Ordering::Relaxed);
            while n < celt && i < self.ids.len() {
                // SAFETY: the caller guarantees rgelt has >= celt slots; write a freshly-built FORMATETC.
                unsafe { rgelt.add(n as usize).write(probe_formatetc(self.ids[i])) };
                i += 1;
                n += 1;
            }
            self.idx.store(i, Ordering::Relaxed);
            if !pceltfetched.is_null() {
                // SAFETY: caller out-param; may be null (then ignored).
                unsafe { *pceltfetched = n };
            }
            if n == celt {
                S_OK
            } else {
                S_FALSE
            }
        })
    }
    fn Skip(&self, celt: u32) -> windows::core::Result<()> {
        use std::sync::atomic::Ordering;
        guard_res(|| {
            let i = (self.idx.load(Ordering::Relaxed) + celt as usize).min(self.ids.len());
            self.idx.store(i, Ordering::Relaxed);
            Ok(())
        })
    }
    fn Reset(&self) -> windows::core::Result<()> {
        use std::sync::atomic::Ordering;
        guard_res(|| {
            self.idx.store(0, Ordering::Relaxed);
            Ok(())
        })
    }
    fn Clone(&self) -> windows::core::Result<windows::Win32::System::Com::IEnumFORMATETC> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        guard_res(|| {
            Ok(ProbeEnum {
                ids: self.ids.clone(),
                idx: AtomicUsize::new(self.idx.load(Ordering::Relaxed)),
            }
            .into())
        })
    }
}

/// A minimal source `IDataObject` for the OLE probe: two byte formats over `TYMED_HGLOBAL`, each
/// `GetData` handing back a FRESH HGLOBAL the caller frees (mirrors `StoreDataObject`).
#[cfg(windows)]
#[windows::core::implement(windows::Win32::System::Com::IDataObject)]
struct ProbeData {
    /// `(cf, bytes)` pairs in served order.
    formats: Vec<(u16, Vec<u8>)>,
}

#[cfg(windows)]
#[allow(non_snake_case)]
impl windows::Win32::System::Com::IDataObject_Impl for ProbeData_Impl {
    fn GetData(
        &self,
        pformatetcin: *const windows::Win32::System::Com::FORMATETC,
    ) -> windows::core::Result<windows::Win32::System::Com::STGMEDIUM> {
        use std::mem::ManuallyDrop;
        use windows::core::Error;
        use windows::Win32::Foundation::{DV_E_FORMATETC, E_OUTOFMEMORY};
        use windows::Win32::System::Com::{STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
        guard_res(|| {
            // SAFETY: OLE guarantees a valid FORMATETC pointer for the call.
            let req = unsafe { &*pformatetcin };
            if (req.tymed & TYMED_HGLOBAL.0 as u32) == 0 {
                return Err(Error::from_hresult(DV_E_FORMATETC));
            }
            let Some((_, bytes)) = self.formats.iter().find(|(cf, _)| *cf == req.cfFormat) else {
                return Err(Error::from_hresult(DV_E_FORMATETC));
            };
            // Hand back a FRESH HGLOBAL — the caller frees it (pUnkForRelease = None).
            // SAFETY: `alloc_global` is a standard GlobalAlloc/Lock/copy/Unlock; the handle is then
            // owned by the returned STGMEDIUM (ReleaseStgMedium frees it).
            let h = unsafe { alloc_global(bytes) };
            if h.0.is_null() {
                return Err(Error::from_hresult(E_OUTOFMEMORY));
            }
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: h },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        })
    }
    fn GetDataHere(
        &self,
        _: *const windows::Win32::System::Com::FORMATETC,
        _: *mut windows::Win32::System::Com::STGMEDIUM,
    ) -> windows::core::Result<()> {
        use windows::core::Error;
        use windows::Win32::Foundation::DV_E_FORMATETC;
        guard_res(|| Err(Error::from_hresult(DV_E_FORMATETC)))
    }
    fn QueryGetData(
        &self,
        pformatetc: *const windows::Win32::System::Com::FORMATETC,
    ) -> windows::core::HRESULT {
        use windows::Win32::Foundation::{DV_E_FORMATETC, S_OK};
        use windows::Win32::System::Com::TYMED_HGLOBAL;
        guard_hr(|| {
            // SAFETY: valid for the call.
            let req = unsafe { &*pformatetc };
            if (req.tymed & TYMED_HGLOBAL.0 as u32) != 0
                && self.formats.iter().any(|(cf, _)| *cf == req.cfFormat)
            {
                S_OK
            } else {
                DV_E_FORMATETC
            }
        })
    }
    fn GetCanonicalFormatEtc(
        &self,
        _: *const windows::Win32::System::Com::FORMATETC,
        pout: *mut windows::Win32::System::Com::FORMATETC,
    ) -> windows::core::HRESULT {
        use windows::Win32::Foundation::E_NOTIMPL;
        guard_hr(|| {
            if !pout.is_null() {
                // SAFETY: caller out-param; ptd=null signals "use the input formatetc".
                unsafe { (*pout).ptd = std::ptr::null_mut() };
            }
            E_NOTIMPL
        })
    }
    fn SetData(
        &self,
        _: *const windows::Win32::System::Com::FORMATETC,
        _: *const windows::Win32::System::Com::STGMEDIUM,
        _: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        use windows::core::Error;
        use windows::Win32::Foundation::E_NOTIMPL;
        guard_res(|| Err(Error::from_hresult(E_NOTIMPL)))
    }
    fn EnumFormatEtc(
        &self,
        dwdirection: u32,
    ) -> windows::core::Result<windows::Win32::System::Com::IEnumFORMATETC> {
        use std::sync::atomic::AtomicUsize;
        use windows::core::Error;
        use windows::Win32::Foundation::E_NOTIMPL;
        use windows::Win32::System::Com::DATADIR_GET;
        guard_res(|| {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(Error::from_hresult(E_NOTIMPL));
            }
            Ok(ProbeEnum {
                ids: self.formats.iter().map(|(cf, _)| *cf).collect(),
                idx: AtomicUsize::new(0),
            }
            .into())
        })
    }
    fn DAdvise(
        &self,
        _: *const windows::Win32::System::Com::FORMATETC,
        _: u32,
        _: windows::core::Ref<windows::Win32::System::Com::IAdviseSink>,
    ) -> windows::core::Result<u32> {
        use windows::core::Error;
        use windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED;
        guard_res(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        use windows::core::Error;
        use windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED;
        guard_res(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }
    fn EnumDAdvise(&self) -> windows::core::Result<windows::Win32::System::Com::IEnumSTATDATA> {
        use windows::core::Error;
        use windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED;
        guard_res(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }
}

/// Decode `bytes` as a NUL-terminated UTF-16LE string (CF_UNICODETEXT payload).
#[cfg(windows)]
fn utf16_to_string(bytes: &[u8]) -> String {
    let units: Vec<u16> =
        bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..len])
}

/// Read the bytes of an HGLOBAL held in `medium` (bounded by `GlobalSize`), then `ReleaseStgMedium`.
///
/// # Safety
/// `medium` must be a valid `TYMED_HGLOBAL` STGMEDIUM returned by `GetData`; we lock/copy/unlock its
/// HGLOBAL and then release the medium (freeing the handle).
#[cfg(windows)]
unsafe fn take_stgmedium_bytes(
    medium: &mut windows::Win32::System::Com::STGMEDIUM,
) -> Option<Vec<u8>> {
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::ReleaseStgMedium;
    let h = medium.u.hGlobal;
    let out = if h.0.is_null() {
        None
    } else {
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            None
        } else {
            let n = GlobalSize(h);
            let v = std::slice::from_raw_parts(p, n).to_vec();
            let _ = GlobalUnlock(h);
            Some(v)
        }
    };
    ReleaseStgMedium(medium as *mut _);
    out
}

// ---------------------------------------------------------------------------
// CF_HDROP round-trip probe (2b).
//
// `clipprobe roundtrip-hdrop` validates that a synthetic `CF_HDROP` blob (DROPFILES header +
// two wide UTF-16 paths) survives the private-clipboard store on both the user32 and OLE surfaces.
// No real files are created — this is a pure byte-transport test. Prints:
//   HDROP-U32-LEN=<n>  (allocated HGLOBAL size; may be rounded up by the heap)
//   HDROP-U32-OK=<true|false>
//   HDROP-OLE-OK=<true|false>
//   PROBE-HDROP-DONE
// ---------------------------------------------------------------------------

/// Build a synthetic CF_HDROP blob: `DROPFILES` header + two wide NUL-terminated paths, double-NUL
/// terminated.  No real files are created; this is a byte-transport fixture.
#[cfg(windows)]
fn make_hdrop_blob() -> Vec<u8> {
    let mut hdrop: Vec<u8> = Vec::new();
    hdrop.extend_from_slice(&20u32.to_le_bytes()); // DROPFILES.pFiles = sizeof(DROPFILES) = 20
    hdrop.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    hdrop.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    hdrop.extend_from_slice(&0i32.to_le_bytes()); // fNC (BOOL)
    hdrop.extend_from_slice(&1i32.to_le_bytes()); // fWide = 1 → UTF-16 paths
    for p in ["C:\\box\\a.txt", "C:\\box\\b.txt"] {
        for u in p.encode_utf16() {
            hdrop.extend_from_slice(&u.to_le_bytes());
        }
        hdrop.extend_from_slice(&0u16.to_le_bytes()); // NUL terminator per path
    }
    hdrop.extend_from_slice(&0u16.to_le_bytes()); // extra NUL → double-NUL list end
    hdrop
}

/// CF_HDROP round-trip via user32 and OLE.
#[cfg(windows)]
fn run_hdrop() -> i32 {
    use windows::Win32::System::Com::{
        CoInitializeEx, IDataObject, COINIT_APARTMENTTHREADED, DVASPECT_CONTENT, FORMATETC,
        TYMED_HGLOBAL,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{OleGetClipboard, OleSetClipboard, ReleaseStgMedium};

    const CF_HDROP: u32 = 15;

    let hdrop = make_hdrop_blob();

    unsafe {
        // OLE requires COM init on this STA thread.
        // SAFETY: standard COM init; we tolerate S_FALSE (already initialized on this thread).
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            eprintln!("FAIL: CoInitializeEx hr={hr:?}");
            return 1;
        }

        // ---- user32 round-trip ----
        // SAFETY: standard user32 clipboard write; the system (or hook) takes ownership of the
        // HGLOBAL on SetClipboardData success, so we do not free it ourselves.
        if OpenClipboard(None).is_err() {
            eprintln!("FAIL: OpenClipboard(u32-write)");
            return 1;
        }
        let _ = EmptyClipboard();
        let h = alloc_global(&hdrop);
        if h.0.is_null() {
            let _ = CloseClipboard();
            eprintln!("FAIL: alloc_global CF_HDROP");
            return 1;
        }
        if SetClipboardData(CF_HDROP, Some(HANDLE(h.0))).is_err() {
            let _ = CloseClipboard();
            eprintln!("FAIL: SetClipboardData CF_HDROP");
            return 1;
        }
        let _ = CloseClipboard();

        // SAFETY: standard user32 clipboard read; the returned handle is owned by the clipboard.
        if OpenClipboard(None).is_err() {
            eprintln!("FAIL: OpenClipboard(u32-read)");
            return 1;
        }
        let rb = read_global_bytes(CF_HDROP).unwrap_or_default();
        let _ = CloseClipboard();

        let u32_ok = rb.len() >= hdrop.len() && rb[..hdrop.len()] == hdrop[..];
        println!("HDROP-U32-LEN={}", rb.len());
        println!("HDROP-U32-OK={u32_ok}");

        // ---- OLE round-trip ----
        let obj: IDataObject = ProbeData {
            formats: vec![(CF_HDROP as u16, hdrop.clone())],
        }
        .into();
        // SAFETY: a boxed in-process IDataObject; OleSetClipboard takes a borrowed reference.
        if let Err(e) = OleSetClipboard(&obj) {
            eprintln!("FAIL: OleSetClipboard CF_HDROP {e:?}");
            return 1;
        }

        // SAFETY: OleGetClipboard returns a (possibly proxied) IDataObject; we drive GetData.
        let got = match OleGetClipboard() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("FAIL: OleGetClipboard {e:?}");
                return 1;
            }
        };
        let fe = FORMATETC {
            cfFormat: CF_HDROP as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        // SAFETY: valid FORMATETC; GetData returns a TYMED_HGLOBAL medium we own and must release.
        let ole_bytes = match got.GetData(&fe) {
            Ok(mut medium) => {
                // Read the HGLOBAL directly (mirrors take_stgmedium_bytes but we need the bytes
                // before releasing, so we inline here for clarity).
                let h = medium.u.hGlobal;
                let out = if h.0.is_null() {
                    None
                } else {
                    let p = GlobalLock(h) as *const u8;
                    if p.is_null() {
                        None
                    } else {
                        let n = GlobalSize(h);
                        let v = std::slice::from_raw_parts(p, n).to_vec();
                        let _ = GlobalUnlock(h);
                        Some(v)
                    }
                };
                ReleaseStgMedium(&mut medium as *mut _);
                out.unwrap_or_default()
            }
            Err(e) => {
                eprintln!("FAIL: OLE GetData CF_HDROP {e:?}");
                return 1;
            }
        };

        let ole_ok = ole_bytes.len() >= hdrop.len() && ole_bytes[..hdrop.len()] == hdrop[..];
        println!("HDROP-OLE-OK={ole_ok}");
        println!("PROBE-HDROP-DONE");
    }
    0
}

/// OLE round-trip probe — see the module-level comment above.
#[cfg(windows)]
fn run_ole() -> i32 {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{
        CoInitializeEx, IDataObject, FORMATETC, COINIT_APARTMENTTHREADED, DVASPECT_CONTENT,
        TYMED_HGLOBAL,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::{OleGetClipboard, OleSetClipboard, CF_UNICODETEXT};

    let cf_text = CF_UNICODETEXT.0;
    let text = "OLE-FROM-BOX";
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let text_bytes: Vec<u8> = utf16.iter().flat_map(|u| u.to_le_bytes()).collect();
    let html_bytes = b"<i>ole</i>".to_vec();

    unsafe {
        // OLE requires COM init on this STA thread.
        // SAFETY: standard COM init; we tolerate S_FALSE (already initialized).
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            eprintln!("FAIL: CoInitializeEx hr={hr:?}");
            return 1;
        }

        let html_w: Vec<u16> = "HTML Format".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a NUL-terminated wide string we own for the duration of the call.
        let html_fmt = RegisterClipboardFormatW(PCWSTR::from_raw(html_w.as_ptr()));
        if html_fmt == 0 || html_fmt > u16::MAX as u32 {
            eprintln!("FAIL: RegisterClipboardFormatW ({html_fmt})");
            return 1;
        }
        let cf_html = html_fmt as u16;

        // --- write via OLE: glass's OleSetClipboard detour marshals the source IDataObject into the
        //     private store. ---
        let obj: IDataObject = ProbeData {
            formats: vec![(cf_text, text_bytes), (cf_html, html_bytes)],
        }
        .into();
        // SAFETY: a boxed in-process IDataObject; OleSetClipboard takes a borrowed reference.
        if let Err(e) = OleSetClipboard(&obj) {
            eprintln!("FAIL: OleSetClipboard {e:?}");
            return 1;
        }

        // --- read back via OLE: glass's OleGetClipboard detour hands back its proxy IDataObject. ---
        // SAFETY: returns a marshaled proxy; we drive GetData per format.
        let got = match OleGetClipboard() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("FAIL: OleGetClipboard {e:?}");
                return 1;
            }
        };
        let mut ole_text = String::new();
        let mut ole_html = String::new();
        for (cf, is_text, slot) in [
            (cf_text, true, &mut ole_text),
            (cf_html, false, &mut ole_html),
        ] {
            let fe = FORMATETC {
                cfFormat: cf,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            };
            // SAFETY: valid FORMATETC; GetData returns a TYMED_HGLOBAL medium we own + must release.
            let mut medium = match got.GetData(&fe) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("FAIL: OLE GetData cf={cf} {e:?}");
                    return 1;
                }
            };
            // SAFETY: medium is a fresh TYMED_HGLOBAL from GetData; take_stgmedium_bytes releases it.
            let bytes = take_stgmedium_bytes(&mut medium).unwrap_or_default();
            *slot = if is_text {
                utf16_to_string(&bytes)
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
        }

        // --- read back via raw user32 (cross-surface coherence: the OLE copy must also be visible to
        //     the user32 serve path). ---
        // SAFETY: standard user32 read; the returned handle is owned by the clipboard (not freed).
        let mut u32_text = String::new();
        if OpenClipboard(None).is_ok() {
            if let Ok(hr) = GetClipboardData(cf_text as u32) {
                if !hr.is_invalid() {
                    let g = windows::Win32::Foundation::HGLOBAL(hr.0);
                    let p = GlobalLock(g) as *const u16;
                    if !p.is_null() {
                        let mut len = 0usize;
                        while *p.add(len) != 0 {
                            len += 1;
                        }
                        u32_text =
                            String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                        let _ = GlobalUnlock(g);
                    }
                }
            }
            let _ = CloseClipboard();
        }

        println!("OLE-TEXT={ole_text}");
        println!("OLE-HTML={ole_html}");
        println!("U32-TEXT={u32_text}");
        println!("PROBE-OLE-DONE");
    }
    0
}

#[cfg(not(windows))]
fn main() {}
