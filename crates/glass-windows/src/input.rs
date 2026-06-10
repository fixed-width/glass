//! Input injection via `SendInput` (Task 7a).
//!
//! The pointer + Unicode-text paths are a port of the validated probe
//! `tools/windows-validation/src/input.rs`; the chord (X keysym -> VK) mapping is
//! new. Coordinates arrive **window-relative** (0,0 = window top-left); we map them
//! to absolute virtual-desktop pixels via [`crate::dpi`] and then to the 0..65535
//! normalized space `MOUSEEVENTF_ABSOLUTE` expects.
//!
//! Runtime lands on a box later; here it only needs to compile clean for the
//! Windows target (`cargo clippy --target x86_64-pc-windows-gnu`).

use glass_core::keys::Modifier;
use glass_core::platform::{KeyEvent, MouseButton, PointerEvent};
use glass_core::{GlassError, Result};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::dpi;
use crate::util::{extended_frame_bounds, raw_to_hwnd};

/// One mouse-wheel notch in `mouseData` units (Win32 `WHEEL_DELTA`).
const WHEEL_DELTA: i32 = 120;

/// Build a `MOUSEINPUT` `INPUT` carrying `dx`/`dy` (normalized 0..65535 coords for
/// absolute moves; `mouseData` left 0 — use [`mouse_wheel`] for wheel events).
fn mouse(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// Build a wheel `INPUT`: `mouse_data` is wheel notches × [`WHEEL_DELTA`], carried in
/// `mouseData` (for `MOUSEEVENTF_WHEEL`/`MOUSEEVENTF_HWHEEL`). `dx`/`dy` are 0.
fn mouse_wheel(mouse_data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Build a `KEYEVENTF_UNICODE` `INPUT` for one UTF-16 code unit (down, or up if `up`).
fn key_unicode(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Build a virtual-key `INPUT` (down, or up if `up`).
fn key_vk(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Submit a batch of `INPUT`s.
///
/// A *partial* short send (some events injected, some dropped by UIPI /
/// foreground-lock) is an environmental best-effort condition, not a hard
/// error — warn like the probe and return `Ok`. A *total* failure, however —
/// zero events injected from a non-empty batch (locked input desktop, UIPI,
/// foreground lock blocking everything) — is indistinguishable from a
/// successful click/keystroke to the agent, which would then proceed on a
/// false premise. The no-silent-fallbacks invariant requires that be a
/// structured error, matching the X11 backend (every XTEST failure → Backend).
fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: `inputs` is a valid slice and the stride is the real `INPUT` size.
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) } as usize;
    if n == 0 {
        return Err(GlassError::Backend(format!(
            "SendInput injected 0/{} events — input blocked (UIPI / foreground lock / \
             locked input desktop); try running elevated",
            inputs.len()
        )));
    }
    if n != inputs.len() {
        eprintln!(
            "glass: SendInput sent {n}/{} events (UIPI/foreground block? run elevated)",
            inputs.len()
        );
    }
    Ok(())
}

/// The `MOUSEEVENTF_*DOWN`/`*UP` flag pair for a button.
fn button_flags(button: MouseButton) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

/// Inject a pointer event into the active window. Coordinates are window-relative.
pub(crate) fn send_pointer(active_hwnd: isize, event: &PointerEvent) -> Result<()> {
    let hwnd = raw_to_hwnd(active_hwnd);
    // Raise+focus first so input lands on the target (best-effort, like the probe).
    let _ = crate::windows::focus_window(hwnd);

    let fb = extended_frame_bounds(hwnd)
        .ok_or_else(|| GlassError::Backend("no window frame bounds for input".into()))?;
    let origin = (fb.left, fb.top);

    // Virtual-screen metrics, read once.
    // SAFETY: GetSystemMetrics is a pure query of system geometry.
    let (v0, vs) = unsafe {
        (
            (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN)),
            (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)),
        )
    };
    let to_norm =
        |x: i32, y: i32| dpi::screen_to_normalized(v0, vs, dpi::window_to_screen(origin, (x, y)));

    const ABS: MOUSE_EVENT_FLAGS =
        MOUSE_EVENT_FLAGS(MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0);

    match *event {
        PointerEvent::Move { x, y } => {
            let (nx, ny) = to_norm(x, y);
            send(&[mouse(nx, ny, MOUSEEVENTF_MOVE | ABS)])?;
        }
        PointerEvent::Click { x, y, button, count, ref modifiers } => {
            let (nx, ny) = to_norm(x, y);
            let (down, up) = button_flags(button);
            let mut inputs = Vec::new();
            for m in modifiers {
                inputs.push(key_vk(modifier_vk(*m), false));
            }
            inputs.push(mouse(nx, ny, MOUSEEVENTF_MOVE | ABS));
            for _ in 0..count.max(1) {
                inputs.push(mouse(nx, ny, down | ABS));
                inputs.push(mouse(nx, ny, up | ABS));
            }
            for m in modifiers.iter().rev() {
                inputs.push(key_vk(modifier_vk(*m), true));
            }
            send(&inputs)?;
        }
        PointerEvent::Drag { from_x, from_y, to_x, to_y, button, ref modifiers } => {
            let path = glass_core::drag_path((from_x, from_y), (to_x, to_y));
            let (down, up) = button_flags(button);
            let mut inputs = Vec::with_capacity(path.len() + 2 + modifiers.len() * 2);
            for m in modifiers {
                inputs.push(key_vk(modifier_vk(*m), false));
            }
            let (nx0, ny0) = to_norm(path[0].0, path[0].1);
            inputs.push(mouse(nx0, ny0, MOUSEEVENTF_MOVE | ABS));
            inputs.push(mouse(nx0, ny0, down | ABS));
            for &(px, py) in &path[1..] {
                let (nx, ny) = to_norm(px, py);
                inputs.push(mouse(nx, ny, MOUSEEVENTF_MOVE | ABS));
            }
            // Release at the LAST path point, not the origin: the ABSOLUTE flag makes the
            // UP event's coords authoritative, so reusing nx0/ny0 would snap the cursor back
            // and drop the drag at (from). drag_path always returns >=1 point; for a
            // zero-length drag last == path[0], so this is correct in the degenerate case too.
            let last = path[path.len() - 1];
            let (nxl, nyl) = to_norm(last.0, last.1);
            inputs.push(mouse(nxl, nyl, up | ABS));
            for m in modifiers.iter().rev() {
                inputs.push(key_vk(modifier_vk(*m), true));
            }
            send(&inputs)?;
        }
        PointerEvent::Scroll { x, y, dx, dy, ref modifiers } => {
            let (nx, ny) = to_norm(x, y);
            // Scroll sign matches x11 (`scroll_button(5=down,4=up, dy)`): there positive
            // `dy` clicks button 5 = scroll DOWN. Windows WHEEL is positive=forward/up, so
            // negate `dy`. Horizontal: x11 `scroll_button(7=right,6=left, dx)` => positive
            // `dx` = right, and Windows HWHEEL positive = right, so `dx` is used as-is.
            let mut inputs = Vec::new();
            for m in modifiers {
                inputs.push(key_vk(modifier_vk(*m), false));
            }
            inputs.push(mouse(nx, ny, MOUSEEVENTF_MOVE | ABS));
            inputs.push(mouse_wheel(-dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL));
            inputs.push(mouse_wheel(dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL));
            for m in modifiers.iter().rev() {
                inputs.push(key_vk(modifier_vk(*m), true));
            }
            send(&inputs)?;
        }
    }
    Ok(())
}

/// Inject a key event into the active window.
pub(crate) fn send_key(active_hwnd: isize, event: &KeyEvent) -> Result<()> {
    let hwnd = raw_to_hwnd(active_hwnd);
    let _ = crate::windows::focus_window(hwnd);

    match event {
        KeyEvent::Text(s) => {
            let mut inputs = Vec::new();
            for unit in s.encode_utf16() {
                inputs.push(key_unicode(unit, false));
                inputs.push(key_unicode(unit, true));
            }
            // `send` no-ops an empty batch, so empty text is a clean Ok.
            send(&inputs)?;
        }
        KeyEvent::Chord(s) => {
            let (mods, keysym) = glass_core::keys::parse_chord(s)?;
            let vk = keysym_to_vk(keysym)
                .ok_or_else(|| GlassError::InvalidKey(format!("key in chord {s:?} has no Windows mapping")))?;
            let mod_vks: Vec<VIRTUAL_KEY> = mods.iter().map(|&m| modifier_vk(m)).collect();
            let mut inputs = Vec::with_capacity(mod_vks.len() * 2 + 2);
            for &mvk in &mod_vks {
                inputs.push(key_vk(mvk, false));
            }
            inputs.push(key_vk(vk, false));
            inputs.push(key_vk(vk, true));
            for &mvk in mod_vks.iter().rev() {
                inputs.push(key_vk(mvk, true));
            }
            send(&inputs)?;
        }
    }
    Ok(())
}

/// The virtual-key for a chord modifier.
fn modifier_vk(m: Modifier) -> VIRTUAL_KEY {
    match m {
        Modifier::Shift => VK_SHIFT,
        Modifier::Control => VK_CONTROL,
        Modifier::Alt => VK_MENU,
        Modifier::Super => VK_LWIN,
    }
}

/// Map an X keysym (the only ones [`glass_core::keys::parse_chord`] can produce) to a
/// Windows virtual-key. `None` if the key has no mapping on the current layout.
///
/// Named/F-keys come from the pure, Linux-tested [`crate::vkmap`]; printable ASCII falls
/// through to `VkKeyScanW` (Windows-only, hence not part of the pure map).
fn keysym_to_vk(keysym: u32) -> Option<VIRTUAL_KEY> {
    if let Some(vk) = crate::vkmap::named_keysym_to_vk(keysym) {
        return Some(VIRTUAL_KEY(vk));
    }
    if (0x20..=0x7e).contains(&keysym) {
        // Printable ASCII: VkKeyScanW's low byte is the base VK (high byte is the shift
        // state, ignored — the chord's modifiers are explicit). -1 = no mapping on the
        // current layout.
        // SAFETY: VkKeyScanW is a pure layout query.
        let r = unsafe { VkKeyScanW(keysym as u16) };
        if r == -1 {
            return None;
        }
        return Some(VIRTUAL_KEY((r as u16) & 0x00ff));
    }
    None
}
