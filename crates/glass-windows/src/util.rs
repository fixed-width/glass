//! Shared helpers: wide strings, HWND <-> raw, top-level window enumeration,
//! DWM frame bounds / cloaked detection, and the "is this a real app window" filter.

use std::ffi::c_void;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW,
};

/// A snapshot of one top-level window. Stores the HWND as a raw `isize` so the
/// value is `Send` and easy to print; reconstruct with [`raw_to_hwnd`].
#[derive(Clone)]
pub struct WinInfo {
    pub raw: isize,
    pub pid: u32,
    pub title: String,
    pub class: String,
    pub visible: bool,
    pub cloaked: bool,
    pub owned: bool,
    pub toolwindow: bool,
    pub appwindow: bool,
}

impl WinInfo {
    /// The discovery-ladder filter: visible, not cloaked, top-level (un-owned or
    /// explicitly app-window), not a tool/palette window.
    pub fn looks_like_app_window(&self) -> bool {
        self.visible && !self.cloaked && (!self.owned || self.appwindow) && !self.toolwindow
    }

    pub fn hwnd(&self) -> HWND {
        raw_to_hwnd(self.raw)
    }
}

pub fn hwnd_to_raw(h: HWND) -> isize {
    h.0 as isize
}

pub fn raw_to_hwnd(v: isize) -> HWND {
    HWND(v as *mut c_void)
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn get_title(h: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: GetWindowTextW writes at most buf.len() u16s into our stack buffer and
    // returns the count; HWND is only queried. `n` never exceeds buf.len().
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    from_wide(&buf[..n as usize])
}

fn get_class(h: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: GetClassNameW writes at most buf.len() u16s into our stack buffer and
    // returns the count; HWND is only queried. `n` never exceeds buf.len().
    let n = unsafe { GetClassNameW(h, &mut buf) };
    from_wide(&buf[..n as usize])
}

pub fn is_cloaked(h: HWND) -> bool {
    let mut v = 0u32;
    // SAFETY: DwmGetWindowAttribute writes exactly size_of::<u32>() bytes into our stack
    // `v` (the buffer and the size we pass match); HWND is only queried.
    let ok = unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            &mut v as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    ok.is_ok() && v != 0
}

/// The visually-correct outer frame (excludes the invisible resize border) — the
/// rect the real backend uses as the window origin for window-relative coords.
pub fn extended_frame_bounds(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: DwmGetWindowAttribute writes exactly size_of::<RECT>() bytes into our stack
    // `r` (the buffer and the size we pass match); HWND is only queried.
    let ok = unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    ok.is_ok().then_some(r)
}

/// The legacy window rect (includes the invisible resize border on Win10/11).
pub fn window_rect(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: GetWindowRect writes one RECT into our stack `r`; HWND is only queried.
    unsafe { GetWindowRect(h, &mut r) }.ok().map(|_| r)
}

fn collect(h: HWND) -> WinInfo {
    let mut pid = 0u32;
    // SAFETY: GetWindowThreadProcessId writes the owning pid into our stack `pid`; HWND is only queried.
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    // SAFETY: GetWindowLongPtrW reads a window long (GWL_EXSTYLE) from the HWND; no buffer is written.
    let exstyle = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
    // SAFETY: GetWindow returns the owner HWND (or null) for the queried HWND; no buffer is written.
    let owner = unsafe { GetWindow(h, GW_OWNER) };
    let owned = owner.map(|o| !o.0.is_null()).unwrap_or(false);
    WinInfo {
        raw: hwnd_to_raw(h),
        pid,
        title: get_title(h),
        class: get_class(h),
        // SAFETY: IsWindowVisible only queries the HWND's visibility flag; no buffer is written.
        visible: unsafe { IsWindowVisible(h) }.as_bool(),
        cloaked: is_cloaked(h),
        owned,
        toolwindow: exstyle & WS_EX_TOOLWINDOW.0 != 0,
        appwindow: exstyle & WS_EX_APPWINDOW.0 != 0,
    }
}

extern "system" fn enum_cb(h: HWND, lparam: LPARAM) -> BOOL {
    // EnumWindows invokes this across an FFI (C) boundary, so a panic unwinding out
    // of it would be undefined behavior. Catch any panic and keep enumerating (the
    // offending window is simply skipped) rather than let it unwind into C.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: lparam is the &mut Vec<WinInfo> we passed to EnumWindows below.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<WinInfo>) };
        out.push(collect(h));
    }));
    TRUE
}

/// Enumerate every top-level window on the calling thread's desktop.
pub fn enum_top_windows() -> Vec<WinInfo> {
    let mut out: Vec<WinInfo> = Vec::new();
    // SAFETY: enum_cb only dereferences the &mut Vec we pass and is alive for the call.
    let _ = unsafe { EnumWindows(Some(enum_cb), LPARAM(&mut out as *mut _ as isize)) };
    out
}

/// App-like windows whose title contains `needle` (case-insensitive).
pub fn find_by_title(needle: &str) -> Vec<WinInfo> {
    let n = needle.to_lowercase();
    enum_top_windows()
        .into_iter()
        .filter(|w| w.looks_like_app_window() && w.title.to_lowercase().contains(&n))
        .collect()
}
