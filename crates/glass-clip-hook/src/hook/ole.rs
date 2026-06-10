//! The 4 `ole32` clipboard detours (2a-ii). Capture marshals the app's `IDataObject` eagerly into
//! the store; serve hands back the baked-snapshot proxy (`dataobject::StoreDataObject`). Like the
//! user32 detours, fully substituted (no trampoline) — so OLE's own internal user32 calls don't
//! double-fire. Every body `catch_unwind`-guarded (a panic across a COM/HRESULT FFI boundary is UB).

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::panic::{catch_unwind, AssertUnwindSafe};

use retour::static_detour;
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::System::Com::{
    IDataObject, IStream, DATADIR_GET, FORMATETC, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::Ole::ReleaseStgMedium;

use crate::proto::{FormatKey, MAX_ITEM_BYTES, MAX_TOTAL_BYTES};

use super::dataobject::StoreDataObject;
use super::{
    key_of, ole32_proc, read_bytes_from_hglobal, store_empty, store_get_all, store_seq,
    store_set_all,
};

type FnOleSet = unsafe extern "system" fn(*mut c_void) -> HRESULT;
type FnOleGet = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type FnOleFlush = unsafe extern "system" fn() -> HRESULT;
type FnOleIsCurrent = unsafe extern "system" fn(*mut c_void) -> HRESULT;

static_detour! {
    static OleSetClipboardHook: unsafe extern "system" fn(*mut c_void) -> HRESULT;
    static OleGetClipboardHook: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
    static OleFlushClipboardHook: unsafe extern "system" fn() -> HRESULT;
    static OleIsCurrentClipboardHook: unsafe extern "system" fn(*mut c_void) -> HRESULT;
}

thread_local! {
    /// (last `OleSetClipboard` object pointer, store seq right after that SetAll) — for
    /// `OleIsCurrentClipboard`'s owner check.
    static LAST_SET: Cell<(isize, u32)> = const { Cell::new((0, 0)) };
}

/// Read a `TYMED_ISTREAM` medium's bytes (Seek(0)+Read loop), capped at `MAX_ITEM_BYTES`.
///
/// # Safety
/// `stream` is a live `IStream` from a `GetData` STGMEDIUM we own until `ReleaseStgMedium`.
unsafe fn read_stream_bytes(stream: &IStream) -> Option<Vec<u8>> {
    stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
    let mut out: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let mut got = 0u32;
        let hr = stream.Read(
            chunk.as_mut_ptr() as *mut c_void,
            chunk.len() as u32,
            Some(&mut got),
        );
        if hr.is_err() || got == 0 {
            break;
        }
        if out.len() + got as usize > MAX_ITEM_BYTES {
            return None; // oversize: reject, never truncate
        }
        out.extend_from_slice(&chunk[..got as usize]);
    }
    Some(out)
}

/// Marshal every HGLOBAL/ISTREAM format of `data` to `(FormatKey, bytes)`, size-capped.
///
/// # Safety
/// `data` is the app's live `IDataObject` (borrowed; we don't Release it).
unsafe fn marshal(data: &IDataObject) -> Vec<(FormatKey, Vec<u8>)> {
    let mut out: Vec<(FormatKey, Vec<u8>)> = Vec::new();
    let mut total = 0usize;
    let Ok(en) = data.EnumFormatEtc(DATADIR_GET.0 as u32) else {
        return out;
    };
    // Bound the enumeration: a non-conformant boxed `IDataObject` whose enumerator never reports
    // exhaustion would otherwise spin this detour forever (capture must never hang). A real
    // clipboard offers a few dozen formats; 256 is generous.
    for _ in 0..256 {
        let mut fe = [FORMATETC::default()];
        let mut fetched = 0u32;
        // The consuming `IEnumFORMATETC::Next` wrapper takes (&mut [FORMATETC], Option<*mut u32>).
        if en.Next(&mut fe, Some(&mut fetched)).is_err() || fetched == 0 {
            break;
        }
        let base = &fe[0];
        for tymed in [TYMED_HGLOBAL, TYMED_ISTREAM] {
            let req = FORMATETC {
                cfFormat: base.cfFormat,
                ptd: base.ptd,
                dwAspect: base.dwAspect,
                lindex: base.lindex,
                tymed: tymed.0 as u32,
            };
            if data.QueryGetData(&req) != S_OK {
                continue;
            }
            let Ok(mut stg) = data.GetData(&req) else {
                continue;
            };
            // A non-conformant GetData may return a different medium than requested; reading the
            // union by the requested tymed would then read the wrong member (UB). Validate first,
            // releasing the medium so there is no leak before we skip.
            if stg.tymed != tymed.0 as u32 {
                ReleaseStgMedium(&mut stg);
                continue;
            }
            let bytes = if tymed == TYMED_HGLOBAL {
                read_bytes_from_hglobal(stg.u.hGlobal)
            } else {
                (*stg.u.pstm).as_ref().and_then(|s| read_stream_bytes(s))
            };
            ReleaseStgMedium(&mut stg);
            if let Some(b) = bytes {
                if b.len() <= MAX_ITEM_BYTES && total + b.len() <= MAX_TOTAL_BYTES {
                    total += b.len();
                    out.push((key_of(base.cfFormat as u32), b));
                }
                break; // captured this format; don't also try ISTREAM
            }
        }
    }
    out
}

fn ole_set_clipboard(p: *mut c_void) -> HRESULT {
    catch_unwind(AssertUnwindSafe(|| {
        if p.is_null() {
            store_empty();
            LAST_SET.with(|c| c.set((0, store_seq())));
            return S_OK;
        }
        // Borrow the app's IDataObject without taking ownership (don't Release it).
        // SAFETY: `p` is a valid IDataObject* the app passed to OleSetClipboard. `ManuallyDrop`
        // prevents the final Release that `from_raw`'s owned interface would otherwise do.
        let data = ManuallyDrop::new(unsafe { IDataObject::from_raw(p) });
        let items = unsafe { marshal(&data) };
        store_set_all(items);
        LAST_SET.with(|c| c.set((p as isize, store_seq())));
        S_OK
    }))
    .unwrap_or(S_OK)
}

fn ole_get_clipboard(pp: *mut *mut c_void) -> HRESULT {
    catch_unwind(AssertUnwindSafe(|| {
        if pp.is_null() {
            return S_OK;
        }
        let obj: IDataObject = StoreDataObject::new(store_get_all()).into();
        // Transfer one ref to the caller (who Releases it).
        // SAFETY: `pp` is a valid out-param; into_raw hands over ownership.
        unsafe { *pp = obj.into_raw() };
        S_OK
    }))
    .unwrap_or(S_OK)
}

fn ole_flush_clipboard() -> HRESULT {
    // We've eagerly stored; flush is a no-op. (Do not call the real OLE — we substitute it.)
    S_OK
}

fn ole_is_current_clipboard(p: *mut c_void) -> HRESULT {
    catch_unwind(AssertUnwindSafe(|| {
        let (last_p, last_seq) = LAST_SET.with(|c| c.get());
        if !p.is_null() && p as isize == last_p && store_seq() == last_seq {
            S_OK
        } else {
            S_FALSE
        }
    }))
    .unwrap_or(S_FALSE)
}

/// Resolve + enable the 4 ole32 detours. Fail-soft (a missing export is skipped).
pub(super) fn install_ole() {
    // SAFETY: each `ole32_proc` returns the export's absolute address, transmuted to its exact ABI;
    // retour initialize/enable are unsafe by contract; every step is fallible and stays fail-soft.
    unsafe {
        if let Some(p) = ole32_proc(b"OleSetClipboard\0") {
            let t: FnOleSet = std::mem::transmute(p);
            if OleSetClipboardHook.initialize(t, ole_set_clipboard).is_ok() {
                let _ = OleSetClipboardHook.enable();
            }
        }
        if let Some(p) = ole32_proc(b"OleGetClipboard\0") {
            let t: FnOleGet = std::mem::transmute(p);
            if OleGetClipboardHook.initialize(t, ole_get_clipboard).is_ok() {
                let _ = OleGetClipboardHook.enable();
            }
        }
        if let Some(p) = ole32_proc(b"OleFlushClipboard\0") {
            let t: FnOleFlush = std::mem::transmute(p);
            if OleFlushClipboardHook.initialize(t, ole_flush_clipboard).is_ok() {
                let _ = OleFlushClipboardHook.enable();
            }
        }
        if let Some(p) = ole32_proc(b"OleIsCurrentClipboard\0") {
            let t: FnOleIsCurrent = std::mem::transmute(p);
            if OleIsCurrentClipboardHook
                .initialize(t, ole_is_current_clipboard)
                .is_ok()
            {
                let _ = OleIsCurrentClipboardHook.enable();
            }
        }
    }
}
