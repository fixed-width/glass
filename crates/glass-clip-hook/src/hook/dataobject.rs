//! A hand-rolled COM `IDataObject` (+ `IEnumFORMATETC`) that serves a baked clipboard snapshot.
//!
//! Built by `OleGetClipboard` from a one-shot `store_get_all()` so its `GetData` does ZERO pipe I/O
//! (no COM reentrancy on paste). Serves byte formats over `TYMED_HGLOBAL`: stored verbatim, or a
//! byte-synthesized derivative (text triad via code page, `CF_DIBV5` via `dib`). GDI `CF_BITMAP` is
//! NOT served here (the user32 path handles that). Each `GetData` returns a FRESH HGLOBAL the caller
//! frees — never our owned bytes.

#![allow(non_snake_case)]

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{implement, Error, Ref, Result, BOOL, HRESULT};
use windows::Win32::Foundation::{
    DV_E_FORMATETC, E_NOTIMPL, E_OUTOFMEMORY, E_UNEXPECTED, OLE_E_ADVISENOTSUPPORTED, S_FALSE,
    S_OK,
};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA,
    FORMATETC, STGMEDIUM, STGMEDIUM_0, DATADIR_GET, DVASPECT_CONTENT, TYMED_HGLOBAL,
};

use crate::proto::FormatKey;

use super::{
    alloc_hglobal_bytes, id_of, key_of, locale_blob, unicode_to_codepage, CF_DIBV5, CF_LOCALE,
    CF_OEMTEXT, CF_TEXT,
};

/// Run a vtable-method body, converting a panic (UB across the `extern "system"` COM boundary) into
/// an error HRESULT. The macro's thunks have no panic guard and member-crate `panic=abort` is
/// ignored by Cargo, so every `_Impl` method must guard itself (design D7).
fn guard_hr(f: impl FnOnce() -> HRESULT) -> HRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(E_UNEXPECTED)
}
fn guard_res<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(E_UNEXPECTED.into()))
}

/// Bytes for `cf` from the baked `entries`: stored verbatim, or byte-synthesized from its canonical.
fn bytes_for(entries: &[(FormatKey, Vec<u8>)], cf: u16) -> Option<Vec<u8>> {
    let key = key_of(cf as u32);
    if let Some((_, b)) = entries.iter().find(|(k, _)| *k == key) {
        return Some(b.clone());
    }
    let canon = crate::synth::canonical_for(&key)?;
    let (_, src) = entries.iter().find(|(k, _)| *k == canon)?;
    Some(match cf as u32 {
        CF_TEXT => unicode_to_codepage(src, false),
        CF_OEMTEXT => unicode_to_codepage(src, true),
        CF_LOCALE => locale_blob(),
        CF_DIBV5 => crate::dib::dib_to_dibv5(src)?,
        _ => return None,
    })
}

/// Build a fresh HGLOBAL/content `FORMATETC` for `cf` (rebuilt per-id rather than stored/cloned).
fn formatetc(cf: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// The enumerator over the proxy's served format ids. `AtomicUsize` cursor (the macro is Agile →
/// Send+Sync); ids (`u16`) stored, `FORMATETC` built on demand.
#[implement(IEnumFORMATETC)]
struct StoreEnum {
    ids: Vec<u16>,
    idx: AtomicUsize,
}

impl IEnumFORMATETC_Impl for StoreEnum_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        guard_hr(|| {
            let mut n = 0u32;
            let mut i = self.idx.load(Ordering::Relaxed);
            while n < celt && i < self.ids.len() {
                // SAFETY: the caller guarantees rgelt has >= celt slots; write a freshly-built FORMATETC.
                unsafe { rgelt.add(n as usize).write(formatetc(self.ids[i])) };
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
    fn Skip(&self, celt: u32) -> Result<()> {
        guard_res(|| {
            let i = (self.idx.load(Ordering::Relaxed) + celt as usize).min(self.ids.len());
            self.idx.store(i, Ordering::Relaxed);
            Ok(())
        })
    }
    fn Reset(&self) -> Result<()> {
        guard_res(|| {
            self.idx.store(0, Ordering::Relaxed);
            Ok(())
        })
    }
    fn Clone(&self) -> Result<IEnumFORMATETC> {
        guard_res(|| {
            Ok(StoreEnum {
                ids: self.ids.clone(),
                idx: AtomicUsize::new(self.idx.load(Ordering::Relaxed)),
            }
            .into())
        })
    }
}

/// The proxy data object. Holds the baked snapshot + its precomputed served format ids.
// Constructed by the OLE detour path (Task 4, `ole.rs`); suppress dead-code until that lands.
#[allow(dead_code)]
#[implement(IDataObject)]
pub(super) struct StoreDataObject {
    entries: Vec<(FormatKey, Vec<u8>)>,
    ids: Vec<u16>,
}

#[allow(dead_code)] // removed in Task 4 (ole.rs calls it)
impl StoreDataObject {
    /// Build from a store snapshot: served ids = `synth::serve_keys` mapped to this session's ids.
    pub(super) fn new(entries: Vec<(FormatKey, Vec<u8>)>) -> Self {
        let keys: Vec<FormatKey> = entries.iter().map(|(k, _)| k.clone()).collect();
        let ids = crate::synth::serve_keys(&keys)
            .iter()
            .map(id_of)
            .filter(|&id| id != 0 && id <= u16::MAX as u32)
            .map(|id| id as u16)
            .collect();
        Self { entries, ids }
    }
}

impl IDataObject_Impl for StoreDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
        guard_res(|| {
            // SAFETY: OLE guarantees a valid FORMATETC pointer for the call.
            let req = unsafe { &*pformatetcin };
            if (req.tymed & TYMED_HGLOBAL.0 as u32) == 0 {
                return Err(Error::from_hresult(DV_E_FORMATETC));
            }
            let Some(bytes) = bytes_for(&self.entries, req.cfFormat) else {
                return Err(Error::from_hresult(DV_E_FORMATETC));
            };
            // Hand back a FRESH HGLOBAL — the caller frees it (pUnkForRelease = None).
            let Some(h) = alloc_hglobal_bytes(&bytes) else {
                return Err(Error::from_hresult(E_OUTOFMEMORY));
            };
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: h },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        })
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        guard_res(|| Err(Error::from_hresult(DV_E_FORMATETC)))
    }
    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        guard_hr(|| {
            // SAFETY: valid for the call.
            let req = unsafe { &*pformatetc };
            if (req.tymed & TYMED_HGLOBAL.0 as u32) != 0
                && bytes_for(&self.entries, req.cfFormat).is_some()
            {
                S_OK
            } else {
                DV_E_FORMATETC
            }
        })
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, pout: *mut FORMATETC) -> HRESULT {
        guard_hr(|| {
            if !pout.is_null() {
                // SAFETY: caller out-param; ptd=null signals "use the input formatetc".
                unsafe { (*pout).ptd = std::ptr::null_mut() };
            }
            E_NOTIMPL
        })
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> Result<()> {
        guard_res(|| Err(Error::from_hresult(E_NOTIMPL)))
    }
    fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
        guard_res(|| {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(Error::from_hresult(E_NOTIMPL));
            }
            Ok(StoreEnum {
                ids: self.ids.clone(),
                idx: AtomicUsize::new(0),
            }
            .into())
        })
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> Result<u32> {
        guard_res(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }
    fn DUnadvise(&self, _: u32) -> Result<()> {
        guard_res(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }
    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        guard_res(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }
}
