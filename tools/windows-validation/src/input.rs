//! SendInput probe (validation item 3): raise+focus a window, click its centre with
//! absolute virtual-desktop coordinates, then type a string with KEYEVENTF_UNICODE.

use anyhow::bail;
use windows::Win32::Foundation::{HWND, TRUE};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetSystemMetrics, GetWindowThreadProcessId, IsIconic, SetForegroundWindow,
    ShowWindow, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SW_RESTORE,
};

use crate::util::{extended_frame_bounds, find_by_title};

pub fn run(needle: &str) -> anyhow::Result<()> {
    let mut hits = find_by_title(needle);
    if hits.is_empty() {
        bail!("no app window title contains {needle:?}");
    }
    let w = hits.remove(0);
    let hwnd = w.hwnd();
    println!("target: '{}'  (class {})  pid {}", w.title, w.class, w.pid);

    focus(hwnd);

    let Some(r) = extended_frame_bounds(hwnd) else {
        bail!("could not read window frame bounds");
    };
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    println!("clicking window centre at screen ({cx}, {cy})");
    click_absolute(cx, cy);

    let text = "glass";
    println!("typing {text:?} via KEYEVENTF_UNICODE");
    type_text(text);

    println!("\nVerify: the click landed on the window centre and {text:?} appeared.");
    println!("(Capture the window with `winval capture {needle}` to confirm without looking.)");
    Ok(())
}

fn focus(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        // Foreground-lock blocked it: attach to the target's thread input and retry.
        let me = GetCurrentThreadId();
        let target = GetWindowThreadProcessId(hwnd, None);
        let _ = AttachThreadInput(me, target, TRUE);
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        let _ = AttachThreadInput(me, target, false);
    }
}

/// Map a screen pixel to the 0..65535 normalised virtual-desktop space and click.
fn click_absolute(x: i32, y: i32) {
    let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let nx = (((x - vx) as i64 * 65535) / (vw.max(2) as i64 - 1)) as i32;
    let ny = (((y - vy) as i64 * 65535) / (vh.max(2) as i64 - 1)) as i32;

    let mv = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    send(&[
        mouse(nx, ny, mv),
        mouse(nx, ny, mv | MOUSEEVENTF_LEFTDOWN),
        mouse(nx, ny, mv | MOUSEEVENTF_LEFTUP),
    ]);
}

fn type_text(s: &str) {
    let mut inputs = Vec::new();
    for unit in s.encode_utf16() {
        inputs.push(key(unit, KEYEVENTF_UNICODE));
        inputs.push(key(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    send(&inputs);
}

fn mouse(dx: i32, dy: i32, flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn key(scan: u16, flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(inputs: &[INPUT]) {
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        eprintln!("warning: SendInput sent {n}/{} events (UIPI block? run elevated)", inputs.len());
    }
}
