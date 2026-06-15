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

/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`: normalized (0..65535) coordinates
/// over the whole virtual desktop — what every absolute mouse `INPUT` here uses.
const ABS: MOUSE_EVENT_FLAGS = MOUSE_EVENT_FLAGS(MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0);

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

/// Lets `glass_core::run_drag` drive a Windows drag through `SendInput`. Unlike the old
/// single-batch drag, each primitive is its own `SendInput`, so `run_drag` paces the
/// motion over `duration_ms` and dwells at the endpoint before releasing. With
/// `MOUSEEVENTF_ABSOLUTE` a button event's coords are authoritative, so the sink presses
/// and releases at the last position it moved to (the press at the start, the release at
/// the re-asserted endpoint).
struct WindowsDragSink<'a> {
    origin: (i32, i32),
    v0: (i32, i32),
    vs: (i32, i32),
    down: MOUSE_EVENT_FLAGS,
    up: MOUSE_EVENT_FLAGS,
    mods: &'a [Modifier],
    /// Last normalized position emitted by `place`/`move_to`. `button` fires there,
    /// because with `MOUSEEVENTF_ABSOLUTE` the up/down event's own coords are
    /// authoritative — releasing without this would snap the cursor to (0,0) and drop
    /// the drag at the desktop origin. `run_drag` always calls `place` before any
    /// `button`, so the `(0, 0)` seed is overwritten before it is ever read.
    last: (i32, i32),
}

impl WindowsDragSink<'_> {
    fn norm(&self, x: i32, y: i32) -> (i32, i32) {
        dpi::screen_to_normalized(self.v0, self.vs, dpi::window_to_screen(self.origin, (x, y)))
    }
}

impl glass_core::DragSink for WindowsDragSink<'_> {
    fn place(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_to(x, y)
    }
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let (nx, ny) = self.norm(x, y);
        self.last = (nx, ny);
        send(&[mouse(nx, ny, MOUSEEVENTF_MOVE | ABS)])
    }
    fn button(&mut self, down: bool) -> Result<()> {
        let (nx, ny) = self.last;
        let flag = if down { self.down } else { self.up };
        send(&[mouse(nx, ny, flag | ABS)])
    }
    fn modifiers(&mut self, down: bool) -> Result<()> {
        if self.mods.is_empty() {
            return Ok(());
        }
        let mut inputs = Vec::with_capacity(self.mods.len());
        if down {
            for m in self.mods {
                inputs.push(key_vk(modifier_vk(*m), false));
            }
        } else {
            for m in self.mods.iter().rev() {
                inputs.push(key_vk(modifier_vk(*m), true));
            }
        }
        send(&inputs)
    }
}

/// `ChordSink` for Windows: one `SendInput` per call (its own commit), so `run_chord`'s dwell lands
/// between phases the app actually processes as separate frames. `key_vk(_, true)` is the release.
struct WindowsChordSink {
    mod_vks: Vec<VIRTUAL_KEY>,
    vk: VIRTUAL_KEY,
}

impl glass_core::ChordSink for WindowsChordSink {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        let inputs: Vec<_> = if down {
            self.mod_vks.iter().map(|&m| key_vk(m, false)).collect()
        } else {
            self.mod_vks.iter().rev().map(|&m| key_vk(m, true)).collect()
        };
        send(&inputs)
    }
    fn key(&mut self, down: bool) -> Result<()> {
        send(&[key_vk(self.vk, !down)])
    }
}

/// `ScrollSink` for Windows: one `SendInput` per call (its own commit). `wheel` positions the cursor
/// then emits the vertical and horizontal wheel in a single batch; `modifiers` presses/releases the
/// held modifier keys around it, so with `run_scroll`'s dwell the wheel lands in a frame the app reads
/// the modifier as held (instead of released by a same-frame modifier-up).
struct WindowsScrollSink {
    nx: i32,
    ny: i32,
    dx: i32,
    dy: i32,
    mod_vks: Vec<VIRTUAL_KEY>,
}

impl glass_core::ScrollSink for WindowsScrollSink {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        let inputs: Vec<_> = if down {
            self.mod_vks.iter().map(|&m| key_vk(m, false)).collect()
        } else {
            self.mod_vks.iter().rev().map(|&m| key_vk(m, true)).collect()
        };
        send(&inputs)
    }
    fn wheel(&mut self) -> Result<()> {
        // Scroll sign matches x11 (`scroll_button(5=down,4=up, dy)`): there positive `dy` clicks
        // button 5 = scroll DOWN. Windows WHEEL is positive=forward/up, so negate `dy`. Horizontal:
        // positive `dx` = right, and Windows HWHEEL positive = right, so `dx` is used as-is.
        send(&[
            mouse(self.nx, self.ny, MOUSEEVENTF_MOVE | ABS),
            mouse_wheel(-self.dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL),
            mouse_wheel(self.dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL),
        ])
    }
}

/// `TypeSink` for Windows: one `SendInput` per character (its own commit), so `run_type`'s
/// inter-character dwell lands between keystrokes the app processes separately. Bursting the
/// whole string into a single `SendInput` corrupts runs of adjacent identical characters (the
/// tail collapses to the string's last char) — see glass_core::run_type.
struct WindowsTypeSink;

/// Inter-character typing dwell, overridable via `GLASS_TYPE_DWELL_MS` (milliseconds) for
/// slow/loaded hosts (raise it) or fast ones (lower it); defaults to `glass_core::TYPE_DWELL`.
fn type_dwell() -> std::time::Duration {
    std::env::var("GLASS_TYPE_DWELL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(glass_core::TYPE_DWELL)
}

impl glass_core::TypeSink for WindowsTypeSink {
    fn character(&mut self, code_units: &[u16]) -> Result<()> {
        let mut inputs = Vec::with_capacity(code_units.len() * 2);
        for &unit in code_units {
            inputs.push(key_unicode(unit, false));
            inputs.push(key_unicode(unit, true));
        }
        send(&inputs)
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
        PointerEvent::Drag { from_x, from_y, to_x, to_y, button, ref modifiers, duration_ms } => {
            let gesture =
                glass_core::DragGesture::plan((from_x, from_y), (to_x, to_y), duration_ms);
            let (down, up) = button_flags(button);
            let mut sink =
                WindowsDragSink { origin, v0, vs, down, up, mods: modifiers, last: (0, 0) };
            glass_core::run_drag(&mut sink, &gesture)?;
        }
        PointerEvent::Scroll { x, y, dx, dy, ref modifiers } => {
            let (nx, ny) = to_norm(x, y);
            let mod_vks: Vec<VIRTUAL_KEY> = modifiers.iter().map(|&m| modifier_vk(m)).collect();
            // Shared, frame-aware sequencing: hold the modifier across the wheel's frame instead of
            // bursting modifier+wheel+release into one — see glass_core::run_scroll.
            let mut sink = WindowsScrollSink { nx, ny, dx, dy, mod_vks };
            glass_core::run_scroll(&mut sink, !modifiers.is_empty())?;
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
            // One SendInput per character, paced by an inter-character dwell. Injecting the
            // whole string faster than the target drains it races a downstream OS bug that
            // collapses a run of characters to the last one — see glass_core::run_type.
            // (Empty text is a clean Ok: no characters to emit.)
            let mut sink = WindowsTypeSink;
            glass_core::run_type(&mut sink, s, type_dwell())?;
        }
        KeyEvent::Chord(s) => {
            let (mods, keysym) = glass_core::keys::parse_chord(s)?;
            let vk = keysym_to_vk(keysym)
                .ok_or_else(|| GlassError::InvalidKey(format!("key in chord {s:?} has no Windows mapping")))?;
            let mod_vks: Vec<VIRTUAL_KEY> = mods.iter().map(|&m| modifier_vk(m)).collect();
            // Shared, frame-aware sequencing: hold the modifier across the key's frame instead of
            // bursting the whole chord into one — see glass_core::run_chord.
            let mut sink = WindowsChordSink { mod_vks, vk };
            glass_core::run_chord(&mut sink)?;
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
