use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use glass_core::{AxObservationWindow, WindowId};
use windows::Win32::Foundation::{HANDLE, HWND, WAIT_TIMEOUT};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
use windows::Win32::System::Threading::{
    GetProcessIdOfThread, OpenProcess, OpenThread, PROCESS_SYNCHRONIZE,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SYNCHRONIZE, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_PER_MONITOR_AWARE, GetAwarenessFromDpiAwarenessContext, GetDpiForWindow,
    GetProcessDpiAwareness, GetThreadDpiAwarenessContext, PROCESS_PER_MONITOR_DPI_AWARE,
};
use windows::Win32::UI::Shell::GetScaleFactorForMonitor;
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
};

use crate::util::{cloaked, extended_frame_bounds, raw_to_hwnd, window_rect};
use crate::windows::rect_to_geometry;

#[derive(PartialEq, Eq)]
struct DisplayBasis {
    monitor: isize,
    scale_percent: i32,
    window_dpi: u32,
}

pub(crate) struct WindowObservation {
    raw: isize,
    pid: u32,
    tid: u32,
    process: OwnedHandle,
    thread: OwnedHandle,
    display: Option<DisplayBasis>,
    instance: u64,
}

impl WindowObservation {
    pub(crate) fn adopt(raw: isize, expected_pid: u32) -> Option<Self> {
        let (pid, tid) = window_owner(raw_to_hwnd(raw))?;
        if pid != expected_pid {
            return None;
        }
        // SAFETY: a null process handle queries this process's default DPI awareness.
        if unsafe { GetProcessDpiAwareness(None) }.ok()? != PROCESS_PER_MONITOR_DPI_AWARE {
            return None;
        }
        // SAFETY: these handles grant identity queries and zero-time waits only. Each
        // successful open transfers a handle to OwnedHandle immediately.
        let (process, thread) = unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok()?;
            let process = OwnedHandle::from_raw_handle(process.0);
            let thread = OpenThread(
                THREAD_QUERY_LIMITED_INFORMATION | THREAD_SYNCHRONIZE,
                false,
                tid,
            )
            .ok()?;
            let thread = OwnedHandle::from_raw_handle(thread.0);
            if GetProcessIdOfThread(HANDLE(thread.as_raw_handle())) != pid {
                return None;
            }
            (process, thread)
        };
        let observation = Self {
            raw,
            pid,
            tid,
            process,
            thread,
            display: None,
            instance: 0,
        };
        observation.owner_matches().then_some(observation)
    }

    pub(crate) fn owner_matches(&self) -> bool {
        window_owner(raw_to_hwnd(self.raw)) == Some((self.pid, self.tid))
            && still_running(&self.process)
            && still_running(&self.thread)
    }

    pub(crate) fn sample(&mut self) -> Option<AxObservationWindow> {
        let hwnd = raw_to_hwnd(self.raw);
        // SAFETY: these queries inspect the calling thread's DPI awareness and the HWND.
        if unsafe {
            GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext())
                != DPI_AWARENESS_PER_MONITOR_AWARE
                || !IsWindowVisible(hwnd).as_bool()
                || IsIconic(hwnd).as_bool()
                || GetAncestor(hwnd, GA_ROOT) != hwnd
        } || cloaked(hwnd)?
        {
            return None;
        }
        let outer_frame = window_rect(hwnd)?;
        let frame = extended_frame_bounds(hwnd)?;
        if window_rect(hwnd)? != outer_frame
            || outer_frame.left > frame.left
            || outer_frame.top > frame.top
            || outer_frame.right < frame.right
            || outer_frame.bottom < frame.bottom
        {
            return None;
        }
        let geometry = rect_to_geometry(frame);
        if geometry.width == 0 || geometry.height == 0 {
            return None;
        }
        // SAFETY: MonitorFromRect reads our live DWM rectangle. The DPI and scale calls
        // query the window and monitor respectively. A null monitor means the frame is off-screen.
        let display = unsafe {
            let monitor = MonitorFromRect(&frame, MONITOR_DEFAULTTONULL);
            if monitor.is_invalid() {
                return None;
            }
            DisplayBasis {
                monitor: monitor.0 as isize,
                scale_percent: GetScaleFactorForMonitor(monitor).ok()?.0,
                window_dpi: GetDpiForWindow(hwnd),
            }
        };
        if display.window_dpi == 0 || display.scale_percent <= 0 {
            return None;
        }
        if self.display.as_ref() != Some(&display) {
            self.instance += 1;
            self.display = Some(display);
        }
        Some(AxObservationWindow {
            id: WindowId(self.raw as u64),
            instance: self.instance,
            owner_pid: Some(self.pid),
            geometry: Some(geometry),
            outer_geometry: Some(rect_to_geometry(outer_frame)),
            subject: None,
        })
    }
}

fn window_owner(hwnd: HWND) -> Option<(u32, u32)> {
    let mut pid = 0;
    // SAFETY: the query writes the process ID to the stack out-parameter.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0 && tid != 0).then_some((pid, tid))
}

fn still_running(handle: &OwnedHandle) -> bool {
    // SAFETY: the borrowed process or thread handle stays open and a zero timeout never blocks.
    unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), 0) == WAIT_TIMEOUT }
}
