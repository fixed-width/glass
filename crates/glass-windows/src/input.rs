//! Input injection via `SendInput`.
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
use glass_core::{Deadline, GlassError, Result};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT,
    VkKeyScanW,
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
const ABS: MOUSE_EVENT_FLAGS =
    MOUSE_EVENT_FLAGS(MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0);

/// Build a `MOUSEINPUT` `INPUT` carrying `dx`/`dy` (normalized 0..65535 coords for
/// absolute moves; `mouseData` left 0 — use [`mouse_wheel`] for wheel events).
fn mouse(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
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
    let flags = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
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

struct SendFailure<'a> {
    error: GlassError,
    inserted: &'a [INPUT],
}

fn send_prefix_by<'a>(
    inputs: &'a [INPUT],
    inject: &mut impl FnMut(&[INPUT]) -> usize,
) -> std::result::Result<(), SendFailure<'a>> {
    if inputs.is_empty() {
        return Ok(());
    }
    let n = inject(inputs);
    if n == 0 {
        return Err(SendFailure {
            error: GlassError::Backend(format!(
                "SendInput injected 0/{} events — input blocked (UIPI / foreground lock / \
                     locked input desktop); try running elevated",
                inputs.len()
            ))
            .before_dispatch(),
            inserted: &inputs[..0],
        });
    }
    if n != inputs.len() {
        return Err(SendFailure {
            error: GlassError::Backend(format!(
                "SendInput injected {n}/{} events; input state is uncertain (UIPI / foreground lock); \
                 try running elevated",
                inputs.len()
            ))
            .after_dispatch(),
            inserted: &inputs[..n],
        });
    }
    Ok(())
}

fn send_by(inputs: &[INPUT], inject: &mut impl FnMut(&[INPUT]) -> usize) -> Result<()> {
    send_prefix_by(inputs, inject).map_err(|failure| failure.error)
}

/// Submit a batch of `INPUT`s. Any nonzero short delivery is an after-dispatch error.
fn send(inputs: &[INPUT]) -> Result<()> {
    let mut inject = |inputs: &[INPUT]| {
        // SAFETY: `inputs` is a valid slice and the stride is the real `INPUT` size.
        (unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) }) as usize
    };
    send_by(inputs, &mut inject)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InjectedInputIdentity {
    Keyboard { vk: u16, scan: u16, flags: u32 },
    MouseButton(u32),
}

struct InjectedDown {
    identity: InjectedInputIdentity,
    release: INPUT,
}

enum InjectedInputTransition {
    Down(InjectedDown),
    Up(InjectedInputIdentity),
}

fn keyboard_transition(input: &INPUT) -> InjectedInputTransition {
    // SAFETY: the INPUT type tag identifies the active union field.
    let mut key = unsafe { input.Anonymous.ki };
    let identity = InjectedInputIdentity::Keyboard {
        vk: key.wVk.0,
        scan: key.wScan,
        flags: key.dwFlags.0 & !KEYEVENTF_KEYUP.0,
    };
    if key.dwFlags.contains(KEYEVENTF_KEYUP) {
        InjectedInputTransition::Up(identity)
    } else {
        key.dwFlags |= KEYEVENTF_KEYUP;
        InjectedInputTransition::Down(InjectedDown {
            identity,
            release: INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 { ki: key },
            },
        })
    }
}

fn mouse_transition(input: &INPUT) -> Option<InjectedInputTransition> {
    // SAFETY: the INPUT type tag identifies the active union field.
    let mouse = unsafe { input.Anonymous.mi };
    let pairs = [
        (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    ];
    for (down, up) in pairs {
        let identity = InjectedInputIdentity::MouseButton(down.0);
        if mouse.dwFlags.contains(up) {
            return Some(InjectedInputTransition::Up(identity));
        }
        if mouse.dwFlags.contains(down) {
            let mut release = mouse;
            release.dwFlags = MOUSE_EVENT_FLAGS((release.dwFlags.0 & !down.0) | up.0);
            return Some(InjectedInputTransition::Down(InjectedDown {
                identity,
                release: INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 { mi: release },
                },
            }));
        }
    }
    None
}

fn input_transition(input: &INPUT) -> Option<InjectedInputTransition> {
    if input.r#type == INPUT_KEYBOARD {
        Some(keyboard_transition(input))
    } else if input.r#type == INPUT_MOUSE {
        mouse_transition(input)
    } else {
        None
    }
}

#[derive(Default)]
struct InjectedInputState {
    downs: Vec<InjectedDown>,
}

impl InjectedInputState {
    fn observe(&mut self, inserted: &[INPUT]) {
        for input in inserted {
            match input_transition(input) {
                Some(InjectedInputTransition::Down(down)) => self.downs.push(down),
                Some(InjectedInputTransition::Up(identity)) => {
                    if let Some(index) = self
                        .downs
                        .iter()
                        .rposition(|down| down.identity == identity)
                    {
                        self.downs.remove(index);
                    }
                }
                None => {}
            }
        }
    }

    fn releases(&self) -> Vec<INPUT> {
        self.downs.iter().rev().map(|down| down.release).collect()
    }

    fn prepare_release(&mut self, release: INPUT) -> Option<INPUT> {
        let Some(InjectedInputTransition::Up(identity)) = input_transition(&release) else {
            debug_assert!(false, "held input release must be an up transition");
            return None;
        };
        let down = self
            .downs
            .iter_mut()
            .rfind(|down| down.identity == identity)?;
        down.release = release;
        Some(release)
    }

    fn is_empty(&self) -> bool {
        self.downs.is_empty()
    }
}

fn send_tracked_by(
    inputs: &[INPUT],
    state: &mut InjectedInputState,
    earlier_dispatch: bool,
    operation: &'static str,
    inject: &mut impl FnMut(&[INPUT]) -> usize,
) -> Result<()> {
    let failure = match send_prefix_by(inputs, inject) {
        Ok(()) => {
            state.observe(inputs);
            return Ok(());
        }
        Err(failure) => failure,
    };
    state.observe(failure.inserted);
    let primary = if earlier_dispatch {
        failure.error.after_dispatch()
    } else {
        failure.error
    };
    let cleanup_inputs = state.releases();
    if cleanup_inputs.is_empty() {
        return Err(primary);
    }
    match send_prefix_by(&cleanup_inputs, inject) {
        Ok(()) => {
            state.observe(&cleanup_inputs);
            Err(primary)
        }
        Err(cleanup) => {
            state.observe(cleanup.inserted);
            Err(GlassError::input_cleanup_failed(
                operation,
                primary,
                cleanup.error,
            ))
        }
    }
}

fn send_balanced_by(
    inputs: &[INPUT],
    operation: &'static str,
    inject: &mut impl FnMut(&[INPUT]) -> usize,
) -> Result<()> {
    let mut state = InjectedInputState::default();
    send_tracked_by(inputs, &mut state, false, operation, inject)
}

fn send_balanced(inputs: &[INPUT], operation: &'static str) -> Result<()> {
    let mut inject = |inputs: &[INPUT]| {
        // SAFETY: `inputs` is a valid slice and the stride is the real `INPUT` size.
        (unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) }) as usize
    };
    send_balanced_by(inputs, operation, &mut inject)
}

fn send_modifiers_by(
    modifiers: &[VIRTUAL_KEY],
    down: bool,
    state: &mut InjectedInputState,
    inject: &mut impl FnMut(&[INPUT]) -> usize,
) -> Result<()> {
    let inputs: Vec<_> = if down {
        debug_assert!(state.is_empty());
        modifiers
            .iter()
            .map(|&modifier| key_vk(modifier, false))
            .collect()
    } else {
        state.releases()
    };
    let earlier_dispatch = !down && !state.is_empty();
    send_tracked_by(
        &inputs,
        state,
        earlier_dispatch,
        "releasing Windows modifier input",
        inject,
    )
}

fn send_modifiers(
    modifiers: &[VIRTUAL_KEY],
    down: bool,
    state: &mut InjectedInputState,
) -> Result<()> {
    let mut inject = |inputs: &[INPUT]| {
        // SAFETY: `inputs` is a valid slice and the stride is the real `INPUT` size.
        (unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) }) as usize
    };
    send_modifiers_by(modifiers, down, state, &mut inject)
}

fn send_held_input_by(
    input: INPUT,
    down: bool,
    state: &mut InjectedInputState,
    operation: &'static str,
    inject: &mut impl FnMut(&[INPUT]) -> usize,
) -> Result<()> {
    let inputs = if down {
        debug_assert!(state.is_empty());
        vec![input]
    } else {
        state.prepare_release(input).into_iter().collect()
    };
    let earlier_dispatch = !state.is_empty();
    send_tracked_by(&inputs, state, earlier_dispatch, operation, inject)
}

fn preserve_earlier_dispatch(result: Result<()>, earlier_dispatch: bool) -> Result<()> {
    result.map_err(|error| {
        if earlier_dispatch {
            error.after_dispatch()
        } else {
            error
        }
    })
}

fn current_input_may_have_dispatched(result: &Result<()>) -> bool {
    result.is_ok()
        || result.as_ref().is_err_and(|error| {
            error.bound_dispatch() == Some(glass_core::BoundDispatch::MayHaveDispatched)
        })
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
    modifier_state: InjectedInputState,
    button_state: InjectedInputState,
    operation_dispatched: bool,
    /// Last normalized position emitted by `place`/`move_to`. `button` fires there,
    /// because with `MOUSEEVENTF_ABSOLUTE` the up/down event's own coords are
    /// authoritative — releasing without this would snap the cursor to (0,0) and drop
    /// the drag at the desktop origin. `run_drag` always calls `place` before any
    /// `button`, so the `(0, 0)` seed is overwritten before it is ever read.
    last: (i32, i32),
    inject: &'a mut dyn FnMut(&[INPUT]) -> usize,
}

impl WindowsDragSink<'_> {
    fn norm(&self, x: i32, y: i32) -> (i32, i32) {
        dpi::screen_to_normalized(self.v0, self.vs, dpi::window_to_screen(self.origin, (x, y)))
    }
}

impl glass_core::DragSink for WindowsDragSink<'_> {
    fn place(&mut self, x: i32, y: i32) -> Result<()> {
        if self.modifier_state.is_empty() && self.button_state.is_empty() {
            self.operation_dispatched = false;
        }
        self.move_to(x, y)
    }
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let (nx, ny) = self.norm(x, y);
        self.last = (nx, ny);
        let earlier_dispatch = self.operation_dispatched;
        let result = send_by(&[mouse(nx, ny, MOUSEEVENTF_MOVE | ABS)], &mut self.inject);
        self.operation_dispatched |= current_input_may_have_dispatched(&result);
        preserve_earlier_dispatch(result, earlier_dispatch)
    }
    fn button(&mut self, down: bool) -> Result<()> {
        let (nx, ny) = self.last;
        let flag = if down { self.down } else { self.up };
        let earlier_dispatch = self.operation_dispatched;
        let has_input = down || !self.button_state.is_empty();
        let result = send_held_input_by(
            mouse(nx, ny, flag | ABS),
            down,
            &mut self.button_state,
            "releasing Windows drag button input",
            &mut self.inject,
        );
        self.operation_dispatched |= has_input && current_input_may_have_dispatched(&result);
        preserve_earlier_dispatch(result, earlier_dispatch)
    }
    fn modifiers(&mut self, down: bool) -> Result<()> {
        let modifiers: Vec<_> = self.mods.iter().copied().map(modifier_vk).collect();
        let earlier_dispatch = self.operation_dispatched;
        let has_input = if down {
            !modifiers.is_empty()
        } else {
            !self.modifier_state.is_empty()
        };
        let result =
            send_modifiers_by(&modifiers, down, &mut self.modifier_state, &mut self.inject);
        self.operation_dispatched |= has_input && current_input_may_have_dispatched(&result);
        preserve_earlier_dispatch(result, earlier_dispatch)
    }
}

/// `ChordSink` for Windows: one `SendInput` per call (its own commit), so `run_chord`'s dwell lands
/// between phases the app actually processes as separate frames. `key_vk(_, true)` is the release.
struct WindowsChordSink<'a> {
    mod_vks: Vec<VIRTUAL_KEY>,
    vk: VIRTUAL_KEY,
    modifier_state: InjectedInputState,
    key_state: InjectedInputState,
    operation_dispatched: bool,
    inject: &'a mut dyn FnMut(&[INPUT]) -> usize,
}

impl glass_core::ChordSink for WindowsChordSink<'_> {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        if down && self.modifier_state.is_empty() && self.key_state.is_empty() {
            self.operation_dispatched = false;
        }
        let earlier_dispatch = self.operation_dispatched;
        let has_input = if down {
            !self.mod_vks.is_empty()
        } else {
            !self.modifier_state.is_empty()
        };
        let result = send_modifiers_by(
            &self.mod_vks,
            down,
            &mut self.modifier_state,
            &mut self.inject,
        );
        self.operation_dispatched |= has_input && current_input_may_have_dispatched(&result);
        preserve_earlier_dispatch(result, earlier_dispatch)
    }
    fn key(&mut self, down: bool) -> Result<()> {
        let earlier_dispatch = self.operation_dispatched;
        let has_input = down || !self.key_state.is_empty();
        let result = send_held_input_by(
            key_vk(self.vk, !down),
            down,
            &mut self.key_state,
            "releasing Windows chord key input",
            &mut self.inject,
        );
        self.operation_dispatched |= has_input && current_input_may_have_dispatched(&result);
        preserve_earlier_dispatch(result, earlier_dispatch)
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
    modifier_state: InjectedInputState,
}

impl glass_core::ScrollSink for WindowsScrollSink {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        send_modifiers(&self.mod_vks, down, &mut self.modifier_state)
    }
    fn wheel(&mut self) -> Result<()> {
        // Scroll sign matches x11 (`scroll_button(5=down,4=up, dy)`): there positive `dy` clicks
        // button 5 = scroll DOWN. Windows WHEEL is positive=forward/up, so negate `dy`. Horizontal:
        // positive `dx` = right, and Windows HWHEEL positive = right, so `dx` is used as-is.
        let vertical = self
            .dy
            .checked_mul(-WHEEL_DELTA)
            .ok_or(GlassError::InvalidPointerInput(
                "vertical scroll delta overflowed",
            ))?;
        let horizontal =
            self.dx
                .checked_mul(WHEEL_DELTA)
                .ok_or(GlassError::InvalidPointerInput(
                    "horizontal scroll delta overflowed",
                ))?;
        send(&[
            mouse(self.nx, self.ny, MOUSEEVENTF_MOVE | ABS),
            mouse_wheel(vertical, MOUSEEVENTF_WHEEL),
            mouse_wheel(horizontal, MOUSEEVENTF_HWHEEL),
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
    fn character(&mut self, c: char) -> Result<()> {
        // All of the char's UTF-16 units (1 for BMP, 2 for a surrogate pair) go in one
        // SendInput so a non-BMP char is committed as a unit.
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        let mut inputs = Vec::with_capacity(units.len() * 2);
        for unit in units.iter().copied() {
            inputs.push(key_unicode(unit, false));
            inputs.push(key_unicode(unit, true));
        }
        send_balanced(&inputs, "releasing Windows text input")
    }
}

pub(crate) fn send_pointer_by(
    active_hwnd: isize,
    event: &PointerEvent,
    deadline: Deadline,
) -> Result<()> {
    glass_core::validate_pointer_input(event)?;
    crate::run_windows_call_by(deadline, "pointer input", |dispatch| {
        // `Gesture` (multi-touch) can never succeed on this backend; reject it before
        // `focus_window`/`extended_frame_bounds`, so it fails fast with `Unsupported` and without
        // raising the target window or masking the call-shape error behind an unrelated
        // frame-bounds `Backend` error (mirrors the macOS backend's early check). The
        // `PointerEvent::Gesture` match arm below stays for exhaustiveness.
        if matches!(event, PointerEvent::Gesture { .. }) {
            return Err(crate::unsupported_multi_touch());
        }
        let hwnd = raw_to_hwnd(active_hwnd);
        // Raise+focus first so input lands on the target (best-effort, like the probe).
        let _ = crate::windows::focus_window(hwnd);
        dispatch.mark();
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed("pointer input"));
        }

        let fb = extended_frame_bounds(hwnd)
            .ok_or_else(|| GlassError::Backend("no window frame bounds for input".into()))?;
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed("pointer input"));
        }
        let origin = (fb.left, fb.top);

        // Virtual-screen metrics, read once.
        // SAFETY: GetSystemMetrics is a pure query of system geometry.
        let (v0, vs) = unsafe {
            (
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                ),
                (
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                ),
            )
        };
        let to_norm = |x: i32, y: i32| {
            dpi::screen_to_normalized(v0, vs, dpi::window_to_screen(origin, (x, y)))
        };

        match *event {
            PointerEvent::Move { x, y } => {
                let (nx, ny) = to_norm(x, y);
                if deadline.has_passed() {
                    return Err(GlassError::caller_deadline_elapsed("pointer input"));
                }
                send(&[mouse(nx, ny, MOUSEEVENTF_MOVE | ABS)])?;
            }
            PointerEvent::Click {
                x,
                y,
                button,
                count,
                ref modifiers,
            } => {
                let (nx, ny) = to_norm(x, y);
                let (down, up) = button_flags(button);
                let click_events = usize::try_from(count)
                    .ok()
                    .and_then(|count| count.checked_mul(2))
                    .ok_or_else(|| {
                        GlassError::InvalidPointerInput("click event count overflowed")
                            .before_dispatch()
                    })?;
                let capacity = modifiers
                    .len()
                    .checked_mul(2)
                    .and_then(|count| count.checked_add(1))
                    .and_then(|count| count.checked_add(click_events))
                    .ok_or_else(|| {
                        GlassError::InvalidPointerInput("click event count overflowed")
                            .before_dispatch()
                    })?;
                let mut inputs = Vec::with_capacity(capacity);
                for m in modifiers {
                    inputs.push(key_vk(modifier_vk(*m), false));
                }
                inputs.push(mouse(nx, ny, MOUSEEVENTF_MOVE | ABS));
                for _ in 0..count {
                    inputs.push(mouse(nx, ny, down | ABS));
                    inputs.push(mouse(nx, ny, up | ABS));
                }
                for m in modifiers.iter().rev() {
                    inputs.push(key_vk(modifier_vk(*m), true));
                }
                if deadline.has_passed() {
                    return Err(GlassError::caller_deadline_elapsed("pointer input"));
                }
                send_balanced(&inputs, "releasing Windows click input")?;
            }
            PointerEvent::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                ref modifiers,
                duration_ms,
            } => {
                let gesture =
                    glass_core::DragGesture::plan((from_x, from_y), (to_x, to_y), duration_ms);
                let (down, up) = button_flags(button);
                let mut inject = |inputs: &[INPUT]| {
                    // SAFETY: `inputs` is a valid slice and the stride is the real `INPUT` size.
                    (unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) }) as usize
                };
                let mut sink = WindowsDragSink {
                    origin,
                    v0,
                    vs,
                    down,
                    up,
                    mods: modifiers,
                    modifier_state: InjectedInputState::default(),
                    button_state: InjectedInputState::default(),
                    operation_dispatched: false,
                    last: (0, 0),
                    inject: &mut inject,
                };
                glass_core::run_drag_by(&mut sink, &gesture, deadline)?;
            }
            PointerEvent::Scroll {
                x,
                y,
                dx,
                dy,
                ref modifiers,
            } => {
                let (nx, ny) = to_norm(x, y);
                let mod_vks: Vec<VIRTUAL_KEY> = modifiers.iter().map(|&m| modifier_vk(m)).collect();
                // Shared, frame-aware sequencing: hold the modifier across the wheel's frame instead of
                // bursting modifier+wheel+release into one — see glass_core::run_scroll.
                let mut sink = WindowsScrollSink {
                    nx,
                    ny,
                    dx,
                    dy,
                    mod_vks,
                    modifier_state: InjectedInputState::default(),
                };
                glass_core::run_scroll_by(&mut sink, !modifiers.is_empty(), deadline)?;
            }
            PointerEvent::Gesture { .. } => {
                return Err(crate::unsupported_multi_touch());
            }
        }
        Ok(())
    })
}

pub(crate) fn send_key_by(active_hwnd: isize, event: &KeyEvent, deadline: Deadline) -> Result<()> {
    crate::run_windows_call_by(deadline, "key input", |dispatch| {
        let hwnd = raw_to_hwnd(active_hwnd);
        let _ = crate::windows::focus_window(hwnd);
        dispatch.mark();
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed("key input"));
        }

        match event {
            KeyEvent::Text(s) => {
                // One SendInput per character, paced by an inter-character dwell. Injecting the
                // whole string faster than the target drains it races a downstream OS bug that
                // collapses a run of characters to the last one — see glass_core::run_type.
                // (Empty text is a clean Ok: no characters to emit.)
                let mut sink = WindowsTypeSink;
                crate::run_windows_type_by(&mut sink, s, type_dwell(), deadline)?;
            }
            KeyEvent::Chord(s) => {
                let (mods, keysym) = glass_core::keys::parse_chord(s)?;
                let vk = keysym_to_vk(keysym).ok_or_else(|| {
                    GlassError::InvalidKey(format!("key in chord {s:?} has no Windows mapping"))
                })?;
                let mod_vks: Vec<VIRTUAL_KEY> = mods.iter().map(|&m| modifier_vk(m)).collect();
                // Shared, frame-aware sequencing: hold the modifier across the key's frame instead of
                // bursting the whole chord into one — see glass_core::run_chord.
                let mut inject = |inputs: &[INPUT]| {
                    // SAFETY: `inputs` is a valid slice and the stride is the real `INPUT` size.
                    (unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) }) as usize
                };
                let mut sink = WindowsChordSink {
                    mod_vks,
                    vk,
                    modifier_state: InjectedInputState::default(),
                    key_state: InjectedInputState::default(),
                    operation_dispatched: false,
                    inject: &mut inject,
                };
                glass_core::run_chord_by(&mut sink, deadline)?;
            }
        }
        Ok(())
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug, PartialEq, Eq)]
    enum InputIdentity {
        Key {
            vk: u16,
            scan: u16,
            unicode: bool,
            up: bool,
        },
        Mouse {
            x: i32,
            y: i32,
            flags: u32,
        },
    }

    fn input_identities(inputs: &[INPUT]) -> Vec<InputIdentity> {
        inputs
            .iter()
            .map(|input| {
                if input.r#type == INPUT_KEYBOARD {
                    // SAFETY: the INPUT type tag identifies the active union field.
                    let key = unsafe { input.Anonymous.ki };
                    InputIdentity::Key {
                        vk: key.wVk.0,
                        scan: key.wScan,
                        unicode: key.dwFlags.contains(KEYEVENTF_UNICODE),
                        up: key.dwFlags.contains(KEYEVENTF_KEYUP),
                    }
                } else {
                    assert_eq!(input.r#type, INPUT_MOUSE);
                    // SAFETY: the INPUT type tag identifies the active union field.
                    let mouse = unsafe { input.Anonymous.mi };
                    InputIdentity::Mouse {
                        x: mouse.dx,
                        y: mouse.dy,
                        flags: mouse.dwFlags.0,
                    }
                }
            })
            .collect()
    }

    // A gesture must fail fast with the clean `Unsupported` before any window lookup: a dummy
    // hwnd (no frame bounds) still yields the backend-named message, not a `Backend` error —
    // this is the behavior the early check in `send_pointer_by` guarantees.
    #[test]
    fn gesture_fails_fast_with_unsupported_before_frame_bounds() {
        let err = send_pointer_by(
            0,
            &PointerEvent::Gesture {
                pointers: vec![],
                duration_ms: 0,
            },
            Deadline::UNBOUNDED,
        );
        assert!(
            matches!(&err, Err(GlassError::Unsupported(_))),
            "expected Unsupported, got {err:?}"
        );
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("windows backend"), "{msg}");
        assert!(msg.contains("multi_touch"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
    }

    #[test]
    fn invalid_pointer_work_fails_before_focus_or_frame_lookup() {
        let events = [
            PointerEvent::Click {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                count: 0,
                modifiers: vec![],
            },
            PointerEvent::Click {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                count: u32::MAX,
                modifiers: vec![],
            },
            PointerEvent::Scroll {
                x: 0,
                y: 0,
                dx: i32::MIN,
                dy: i32::MAX,
                modifiers: vec![],
            },
        ];

        for event in events {
            let error = send_pointer_by(0, &event, Deadline::UNBOUNDED)
                .expect_err("invalid pointer work must fail before Win32 dispatch");
            assert!(matches!(error.cause(), GlassError::InvalidPointerInput(_)));
            assert_eq!(
                error.bound_dispatch(),
                Some(glass_core::BoundDispatch::NotDispatched)
            );
        }
    }

    #[test]
    fn partial_sendinput_is_an_after_dispatch_error() {
        let inputs = [key_vk(VK_SHIFT, false), key_vk(VK_SHIFT, true)];
        let mut inject = |_: &[INPUT]| 1;

        let error = send_by(&inputs, &mut inject).expect_err("a short send must not be success");

        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("1/2"), "{error}");
        assert!(
            error.to_string().contains("input state is uncertain"),
            "{error}"
        );
    }

    #[test]
    fn zero_sendinput_is_a_not_dispatched_error() {
        let inputs = [key_vk(VK_SHIFT, false), key_vk(VK_SHIFT, true)];
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            0
        };

        let error = send_by(&inputs, &mut inject).expect_err("zero delivery must be explicit");

        assert_eq!(calls, [input_identities(&inputs)]);
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn pointer_move_only_prefix_does_not_release_an_uninjected_button() {
        let inputs = [
            mouse(11, 22, MOUSEEVENTF_MOVE | ABS),
            mouse(11, 22, MOUSEEVENTF_LEFTDOWN | ABS),
            mouse(11, 22, MOUSEEVENTF_LEFTUP | ABS),
        ];
        let mut deliveries = VecDeque::from([1]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };

        let error = send_balanced_by(&inputs, "releasing Windows click input", &mut inject)
            .expect_err("partial primary delivery remains an error after cleanup");

        assert_eq!(calls, [input_identities(&inputs)]);
        assert!(matches!(error, GlassError::AfterDispatch(_)));
    }

    #[test]
    fn partial_click_releases_the_injected_button_down_in_order() {
        let inputs = [
            mouse(11, 22, MOUSEEVENTF_MOVE | ABS),
            mouse(11, 22, MOUSEEVENTF_LEFTDOWN | ABS),
            mouse(11, 22, MOUSEEVENTF_LEFTUP | ABS),
        ];
        let cleanup = [mouse(11, 22, MOUSEEVENTF_LEFTUP | ABS)];
        let mut deliveries = VecDeque::from([2, cleanup.len()]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };

        let error = send_balanced_by(&inputs, "releasing Windows click input", &mut inject)
            .expect_err("partial primary delivery remains an error after cleanup");

        assert_eq!(
            calls,
            [input_identities(&inputs), input_identities(&cleanup)]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn partial_text_releases_only_the_unmatched_unicode_down() {
        let high = 0xd83d;
        let low = 0xde00;
        let inputs = [
            key_unicode(high, false),
            key_unicode(high, true),
            key_unicode(low, false),
            key_unicode(low, true),
        ];
        let expected_cleanup = [key_unicode(low, true)];
        let mut deliveries = VecDeque::from([3, expected_cleanup.len()]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };

        let error = send_balanced_by(&inputs, "releasing Windows text input", &mut inject)
            .expect_err("partial text delivery remains an error after cleanup");

        assert_eq!(
            calls,
            [
                input_identities(&inputs),
                input_identities(&expected_cleanup),
            ]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn partial_sendinput_cleanup_failure_preserves_both_errors() {
        let inputs = [key_vk(VK_SHIFT, false), key_vk(VK_SHIFT, true)];
        let mut deliveries = VecDeque::from([1, 0]);
        let mut inject = |_: &[INPUT]| deliveries.pop_front().unwrap();

        let error = send_balanced_by(&inputs, "releasing Windows test input", &mut inject)
            .expect_err("cleanup failure must stay structured");

        let GlassError::InputCleanupFailed {
            operation,
            primary,
            cleanup,
        } = error
        else {
            panic!("both SendInput failures must remain inspectable");
        };
        assert_eq!(operation, "releasing Windows test input");
        assert_eq!(
            primary.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(
            cleanup.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn partial_modifier_down_releases_only_the_injected_prefix_in_reverse_order() {
        let modifiers = [VK_CONTROL, VK_SHIFT, VK_MENU];
        let expected_cleanup = [key_vk(VK_CONTROL, true)];
        let mut deliveries = VecDeque::from([1, expected_cleanup.len()]);
        let mut calls = Vec::new();
        let mut state = InjectedInputState::default();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };

        let error = send_modifiers_by(&modifiers, true, &mut state, &mut inject)
            .expect_err("partial modifier delivery remains an error after cleanup");
        send_modifiers_by(&modifiers, false, &mut state, &mut inject)
            .expect("the core cleanup callback must not release unproven modifiers");

        let modifier_downs = [
            key_vk(VK_CONTROL, false),
            key_vk(VK_SHIFT, false),
            key_vk(VK_MENU, false),
        ];
        assert_eq!(
            calls,
            [
                input_identities(&modifier_downs),
                input_identities(&expected_cleanup),
            ]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn partial_modifier_release_retries_only_the_unmatched_suffix_in_order() {
        let modifiers = [VK_CONTROL, VK_SHIFT, VK_MENU];
        let modifier_downs = [
            key_vk(VK_CONTROL, false),
            key_vk(VK_SHIFT, false),
            key_vk(VK_MENU, false),
        ];
        let modifier_ups = [
            key_vk(VK_MENU, true),
            key_vk(VK_SHIFT, true),
            key_vk(VK_CONTROL, true),
        ];
        let remaining_ups = [key_vk(VK_SHIFT, true), key_vk(VK_CONTROL, true)];
        let mut deliveries = VecDeque::from([modifiers.len(), 1, remaining_ups.len()]);
        let mut calls = Vec::new();
        let mut state = InjectedInputState::default();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };

        send_modifiers_by(&modifiers, true, &mut state, &mut inject)
            .expect("modifier downs establish the state released by the next batch");
        let error = send_modifiers_by(&modifiers, false, &mut state, &mut inject)
            .expect_err("partial modifier release remains an error after cleanup");

        assert_eq!(
            calls,
            [
                input_identities(&modifier_downs),
                input_identities(&modifier_ups),
                input_identities(&remaining_ups),
            ]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn zero_modifier_release_retries_all_proven_downs_and_stays_after_dispatch() {
        let modifiers = [VK_CONTROL, VK_SHIFT, VK_MENU];
        let modifier_downs = [
            key_vk(VK_CONTROL, false),
            key_vk(VK_SHIFT, false),
            key_vk(VK_MENU, false),
        ];
        let modifier_ups = [
            key_vk(VK_MENU, true),
            key_vk(VK_SHIFT, true),
            key_vk(VK_CONTROL, true),
        ];
        let mut deliveries = VecDeque::from([modifiers.len(), 0, modifiers.len()]);
        let mut calls = Vec::new();
        let mut state = InjectedInputState::default();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };

        send_modifiers_by(&modifiers, true, &mut state, &mut inject)
            .expect("modifier downs establish the state released by the next batch");
        let error = send_modifiers_by(&modifiers, false, &mut state, &mut inject)
            .expect_err("zero modifier release remains an error after retry cleanup");

        assert_eq!(
            calls,
            [
                input_identities(&modifier_downs),
                input_identities(&modifier_ups),
                input_identities(&modifier_ups),
            ]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn zero_drag_button_down_cleanup_is_a_noop_and_stays_not_dispatched() {
        let last = (31, 47);
        let button_down = [mouse(last.0, last.1, MOUSEEVENTF_LEFTDOWN | ABS)];
        let mut deliveries = VecDeque::from([0]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsDragSink {
            origin: (0, 0),
            v0: (0, 0),
            vs: (1, 1),
            down: MOUSEEVENTF_LEFTDOWN,
            up: MOUSEEVENTF_LEFTUP,
            mods: &[],
            modifier_state: InjectedInputState::default(),
            button_state: InjectedInputState::default(),
            operation_dispatched: false,
            last,
            inject: &mut inject,
        };

        let error = glass_core::DragSink::button(&mut sink, true)
            .expect_err("zero button-down delivery must remain explicit");
        glass_core::DragSink::button(&mut sink, false)
            .expect("the core cleanup callback must not emit an unmatched button-up");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(calls, [input_identities(&button_down)]);
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn run_drag_by_submits_the_current_endpoint_button_up() {
        let start = (31, 47);
        let end = (113, 197);
        let gesture = glass_core::DragGesture {
            waypoints: vec![start, end],
            step: std::time::Duration::ZERO,
            dwell: std::time::Duration::ZERO,
        };
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            submitted.len()
        };
        let mut sink = WindowsDragSink {
            origin: (0, 0),
            v0: (0, 0),
            vs: (65_536, 65_536),
            down: MOUSEEVENTF_LEFTDOWN,
            up: MOUSEEVENTF_LEFTUP,
            mods: &[],
            modifier_state: InjectedInputState::default(),
            button_state: InjectedInputState::default(),
            operation_dispatched: false,
            last: (0, 0),
            inject: &mut inject,
        };

        glass_core::run_drag_by(&mut sink, &gesture, Deadline::UNBOUNDED)
            .expect("the production drag sequence must release at its endpoint");

        drop(sink);
        assert_eq!(
            calls,
            [
                input_identities(&[mouse(start.0, start.1, MOUSEEVENTF_MOVE | ABS)]),
                input_identities(&[mouse(start.0, start.1, MOUSEEVENTF_LEFTDOWN | ABS)]),
                input_identities(&[mouse(end.0, end.1, MOUSEEVENTF_MOVE | ABS)]),
                input_identities(&[mouse(end.0, end.1, MOUSEEVENTF_MOVE | ABS)]),
                input_identities(&[mouse(end.0, end.1, MOUSEEVENTF_LEFTUP | ABS)]),
            ]
        );
    }

    #[test]
    fn run_drag_by_zero_endpoint_up_retries_that_exact_input_and_reuses_the_sink() {
        let first_start = (31, 47);
        let first_end = (113, 197);
        let second_start = (211, 223);
        let second_end = (251, 263);
        let first = glass_core::DragGesture {
            waypoints: vec![first_start, first_end],
            step: std::time::Duration::ZERO,
            dwell: std::time::Duration::ZERO,
        };
        let second = glass_core::DragGesture {
            waypoints: vec![second_start, second_end],
            step: std::time::Duration::ZERO,
            dwell: std::time::Duration::ZERO,
        };
        let mut deliveries = VecDeque::from([1, 1, 1, 1, 0, 1, 1, 1, 1, 1, 1]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsDragSink {
            origin: (0, 0),
            v0: (0, 0),
            vs: (65_536, 65_536),
            down: MOUSEEVENTF_LEFTDOWN,
            up: MOUSEEVENTF_LEFTUP,
            mods: &[],
            modifier_state: InjectedInputState::default(),
            button_state: InjectedInputState::default(),
            operation_dispatched: false,
            last: (0, 0),
            inject: &mut inject,
        };

        let error = glass_core::run_drag_by(&mut sink, &first, Deadline::UNBOUNDED)
            .expect_err("zero endpoint button-up delivery remains explicit after retry cleanup");
        glass_core::run_drag_by(&mut sink, &second, Deadline::UNBOUNDED)
            .expect("successful retry cleanup must leave the production sink reusable");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(
            calls,
            [
                input_identities(&[mouse(first_start.0, first_start.1, MOUSEEVENTF_MOVE | ABS,)]),
                input_identities(&[mouse(
                    first_start.0,
                    first_start.1,
                    MOUSEEVENTF_LEFTDOWN | ABS,
                )]),
                input_identities(&[mouse(first_end.0, first_end.1, MOUSEEVENTF_MOVE | ABS)]),
                input_identities(&[mouse(first_end.0, first_end.1, MOUSEEVENTF_MOVE | ABS)]),
                input_identities(&[mouse(first_end.0, first_end.1, MOUSEEVENTF_LEFTUP | ABS,)]),
                input_identities(&[mouse(first_end.0, first_end.1, MOUSEEVENTF_LEFTUP | ABS,)]),
                input_identities(&[mouse(
                    second_start.0,
                    second_start.1,
                    MOUSEEVENTF_MOVE | ABS,
                )]),
                input_identities(&[mouse(
                    second_start.0,
                    second_start.1,
                    MOUSEEVENTF_LEFTDOWN | ABS,
                )]),
                input_identities(&[mouse(second_end.0, second_end.1, MOUSEEVENTF_MOVE | ABS,)]),
                input_identities(&[mouse(second_end.0, second_end.1, MOUSEEVENTF_MOVE | ABS,)]),
                input_identities(&[mouse(second_end.0, second_end.1, MOUSEEVENTF_LEFTUP | ABS,)]),
            ]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn run_drag_by_zero_button_down_is_after_placement_and_suppresses_unmatched_up() {
        let first_start = (31, 47);
        let first_end = (113, 197);
        let second_start = (211, 223);
        let second_end = (251, 263);
        let first = glass_core::DragGesture {
            waypoints: vec![first_start, first_end],
            step: std::time::Duration::ZERO,
            dwell: std::time::Duration::ZERO,
        };
        let second = glass_core::DragGesture {
            waypoints: vec![second_start, second_end],
            step: std::time::Duration::ZERO,
            dwell: std::time::Duration::ZERO,
        };
        let mut deliveries = VecDeque::from([1, 0, 1, 1, 1, 1, 1]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsDragSink {
            origin: (0, 0),
            v0: (0, 0),
            vs: (65_536, 65_536),
            down: MOUSEEVENTF_LEFTDOWN,
            up: MOUSEEVENTF_LEFTUP,
            mods: &[],
            modifier_state: InjectedInputState::default(),
            button_state: InjectedInputState::default(),
            operation_dispatched: false,
            last: (0, 0),
            inject: &mut inject,
        };

        let error = glass_core::run_drag_by(&mut sink, &first, Deadline::UNBOUNDED)
            .expect_err("a zero button down after placement must remain explicit");
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        glass_core::run_drag_by(&mut sink, &second, Deadline::UNBOUNDED)
            .expect("cleanup after zero button down must leave the sink reusable");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(
            calls,
            [
                input_identities(&[mouse(first_start.0, first_start.1, MOUSEEVENTF_MOVE | ABS,)]),
                input_identities(&[mouse(
                    first_start.0,
                    first_start.1,
                    MOUSEEVENTF_LEFTDOWN | ABS,
                )]),
                input_identities(&[mouse(
                    second_start.0,
                    second_start.1,
                    MOUSEEVENTF_MOVE | ABS,
                )]),
                input_identities(&[mouse(
                    second_start.0,
                    second_start.1,
                    MOUSEEVENTF_LEFTDOWN | ABS,
                )]),
                input_identities(&[mouse(second_end.0, second_end.1, MOUSEEVENTF_MOVE | ABS,)]),
                input_identities(&[mouse(second_end.0, second_end.1, MOUSEEVENTF_MOVE | ABS,)]),
                input_identities(&[mouse(second_end.0, second_end.1, MOUSEEVENTF_LEFTUP | ABS,)]),
            ]
        );
    }

    #[test]
    fn zero_drag_button_up_retries_only_the_button_and_resets_for_the_next_drag() {
        let last = (31, 47);
        let button_down = [mouse(last.0, last.1, MOUSEEVENTF_LEFTDOWN | ABS)];
        let button_up = [mouse(last.0, last.1, MOUSEEVENTF_LEFTUP | ABS)];
        let mut deliveries = VecDeque::from([1, 0, 1, 1, 1]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsDragSink {
            origin: (0, 0),
            v0: (0, 0),
            vs: (1, 1),
            down: MOUSEEVENTF_LEFTDOWN,
            up: MOUSEEVENTF_LEFTUP,
            mods: &[],
            modifier_state: InjectedInputState::default(),
            button_state: InjectedInputState::default(),
            operation_dispatched: false,
            last,
            inject: &mut inject,
        };

        glass_core::DragSink::button(&mut sink, true)
            .expect("the first button-down establishes delivered state");
        let error = glass_core::DragSink::button(&mut sink, false)
            .expect_err("zero button-up delivery remains an error after retry cleanup");
        glass_core::DragSink::button(&mut sink, true)
            .expect("successful retry cleanup must reset state for the next drag");
        glass_core::DragSink::button(&mut sink, false)
            .expect("the next drag releases exactly its own button-down");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(
            calls,
            [
                input_identities(&button_down),
                input_identities(&button_up),
                input_identities(&button_up),
                input_identities(&button_down),
                input_identities(&button_up),
            ]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn failed_drag_button_up_retry_preserves_primary_and_cleanup_errors() {
        let last = (31, 47);
        let button_down = [mouse(last.0, last.1, MOUSEEVENTF_LEFTDOWN | ABS)];
        let button_up = [mouse(last.0, last.1, MOUSEEVENTF_LEFTUP | ABS)];
        let mut deliveries = VecDeque::from([1, 0, 0]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsDragSink {
            origin: (0, 0),
            v0: (0, 0),
            vs: (1, 1),
            down: MOUSEEVENTF_LEFTDOWN,
            up: MOUSEEVENTF_LEFTUP,
            mods: &[],
            modifier_state: InjectedInputState::default(),
            button_state: InjectedInputState::default(),
            operation_dispatched: false,
            last,
            inject: &mut inject,
        };

        glass_core::DragSink::button(&mut sink, true)
            .expect("button-down establishes the state that both releases fail to clear");
        let error = glass_core::DragSink::button(&mut sink, false)
            .expect_err("failed retry cleanup must stay structured");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(
            calls,
            [
                input_identities(&button_down),
                input_identities(&button_up),
                input_identities(&button_up),
            ]
        );
        let GlassError::InputCleanupFailed {
            operation,
            primary,
            cleanup,
        } = error
        else {
            panic!("both drag-button SendInput failures must remain inspectable");
        };
        assert_eq!(operation, "releasing Windows drag button input");
        assert_eq!(
            primary.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(
            cleanup.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn zero_chord_key_down_cleanup_is_a_noop_and_stays_not_dispatched() {
        let vk = VIRTUAL_KEY(0x1b);
        let key_down = [key_vk(vk, false)];
        let mut deliveries = VecDeque::from([0]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsChordSink {
            mod_vks: vec![],
            vk,
            modifier_state: InjectedInputState::default(),
            key_state: InjectedInputState::default(),
            operation_dispatched: false,
            inject: &mut inject,
        };

        let error = glass_core::ChordSink::key(&mut sink, true)
            .expect_err("zero key-down delivery must remain explicit");
        glass_core::ChordSink::key(&mut sink, false)
            .expect("the core cleanup callback must not emit an unmatched key-up");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(calls, [input_identities(&key_down)]);
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn run_chord_by_zero_key_down_is_after_modifiers_and_suppresses_unmatched_up() {
        let modifier = VK_CONTROL;
        let vk = VIRTUAL_KEY(0x1b);
        let mut deliveries = VecDeque::from([1, 0, 1, 1, 1, 1, 1]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsChordSink {
            mod_vks: vec![modifier],
            vk,
            modifier_state: InjectedInputState::default(),
            key_state: InjectedInputState::default(),
            operation_dispatched: false,
            inject: &mut inject,
        };

        let error = glass_core::run_chord_by(&mut sink, Deadline::UNBOUNDED)
            .expect_err("a zero key down after modifier dispatch must remain explicit");
        glass_core::run_chord_by(&mut sink, Deadline::UNBOUNDED)
            .expect("cleanup after zero key down must leave the production sink reusable");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(
            calls,
            [
                input_identities(&[key_vk(modifier, false)]),
                input_identities(&[key_vk(vk, false)]),
                input_identities(&[key_vk(modifier, true)]),
                input_identities(&[key_vk(modifier, false)]),
                input_identities(&[key_vk(vk, false)]),
                input_identities(&[key_vk(vk, true)]),
                input_identities(&[key_vk(modifier, true)]),
            ]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn zero_chord_key_up_retries_only_the_key_and_resets_for_the_next_chord() {
        let vk = VIRTUAL_KEY(0x1b);
        let key_down = [key_vk(vk, false)];
        let key_up = [key_vk(vk, true)];
        let mut deliveries = VecDeque::from([1, 0, 1, 1, 1]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsChordSink {
            mod_vks: vec![],
            vk,
            modifier_state: InjectedInputState::default(),
            key_state: InjectedInputState::default(),
            operation_dispatched: false,
            inject: &mut inject,
        };

        glass_core::ChordSink::key(&mut sink, true)
            .expect("the first key-down establishes delivered state");
        let error = glass_core::ChordSink::key(&mut sink, false)
            .expect_err("zero key-up delivery remains an error after retry cleanup");
        glass_core::ChordSink::key(&mut sink, true)
            .expect("successful retry cleanup must reset state for the next chord");
        glass_core::ChordSink::key(&mut sink, false)
            .expect("the next chord releases exactly its own key-down");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(
            calls,
            [
                input_identities(&key_down),
                input_identities(&key_up),
                input_identities(&key_up),
                input_identities(&key_down),
                input_identities(&key_up),
            ]
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn failed_chord_key_up_retry_preserves_primary_and_cleanup_errors() {
        let vk = VIRTUAL_KEY(0x1b);
        let key_down = [key_vk(vk, false)];
        let key_up = [key_vk(vk, true)];
        let mut deliveries = VecDeque::from([1, 0, 0]);
        let mut calls = Vec::new();
        let mut inject = |submitted: &[INPUT]| {
            calls.push(input_identities(submitted));
            deliveries.pop_front().unwrap()
        };
        let mut sink = WindowsChordSink {
            mod_vks: vec![],
            vk,
            modifier_state: InjectedInputState::default(),
            key_state: InjectedInputState::default(),
            operation_dispatched: false,
            inject: &mut inject,
        };

        glass_core::ChordSink::key(&mut sink, true)
            .expect("key-down establishes the state that both releases fail to clear");
        let error = glass_core::ChordSink::key(&mut sink, false)
            .expect_err("failed retry cleanup must stay structured");

        drop(sink);
        assert!(deliveries.is_empty());
        assert_eq!(
            calls,
            [
                input_identities(&key_down),
                input_identities(&key_up),
                input_identities(&key_up),
            ]
        );
        let GlassError::InputCleanupFailed {
            operation,
            primary,
            cleanup,
        } = error
        else {
            panic!("both chord-key SendInput failures must remain inspectable");
        };
        assert_eq!(operation, "releasing Windows chord key input");
        assert_eq!(
            primary.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(
            cleanup.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }
}
