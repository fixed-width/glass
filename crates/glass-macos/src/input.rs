#![forbid(unsafe_code)]
//! CGEvent mouse + keyboard injection: `send_pointer` maps each window-relative PIXEL
//! coordinate to a global Quartz POINT via `coords::pixel_to_global_point`, raises the
//! target app (focus-before-inject), then posts `Move`/`Click`/`Drag`/`Scroll` as CGEvents
//! through the HID event tap. `Gesture` (Android multi-touch) is `Unsupported` — see
//! `glass_core::platform::PointerEvent`'s doc. `send_key` does the same focus-before-inject,
//! then posts `KeyEvent::Text`/`Chord` as keyboard CGEvents.
//!
//! Ported from `tools/macos-validation/inject_input.swift`'s
//! `clickGlobal`/`postKey`/`typeString` onto `objc2-core-graphics`'s generated bindings, which
//! expose `CGEvent`'s constructors/accessors as **associated** functions taking
//! `Option<&CGEvent>` (e.g. `CGEvent::post(tap, event)`), not Swift's `self`-methods.
//! header-translator marks all of them (and `NSRunningApplication::activateWithOptions`) plain
//! safe: their only precondition, a live `CGEvent`/`CGEventSource`/`NSRunningApplication`
//! reference, is already enforced by the type system, so this file needs no `unsafe` block.
//!
//! Drag/scroll/text/chord reuse glass_core's deadline-aware shared drivers. macOS represents
//! held modifiers as `CGEventFlags` stamped on payload events, so its drag, scroll, and chord
//! modifier sink hooks update no native state; the shared drivers still own deadline checks,
//! pacing, and key/button release ordering.
//!
//! **Main-thread affinity:** called from `MacosPlatform::send_pointer`/`send_key`, which glass
//! always drives from the main thread.

use std::thread;
use std::time::Duration;

use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton, CGScrollEventUnit,
};

use glass_core::keys::Modifier;
use glass_core::platform::{KeyEvent, MouseButton, PointerEvent};
use glass_core::{
    ChordSink, Deadline, DragGesture, DragSink, GlassError, Result, ScrollSink, TypeSink,
    run_chord_by as run_chord_input_by, run_drag_by as run_drag_input_by,
    run_scroll_by as run_scroll_input_by, run_type_by as run_text_input_by,
};

use crate::coords::pixel_to_global_point;
use crate::input_deadline::{after_focus, require_payload_time, run_scroll_wheel_by};
use crate::keymap;

/// Map a window-relative pixel coordinate to a global Quartz point, accounting for the
/// session's display scale and window origin.
fn to_cgpoint(x: i32, y: i32, scale: f64, origin_pt: (f64, f64)) -> CGPoint {
    let (gx, gy) = pixel_to_global_point((x, y), scale, origin_pt);
    CGPoint { x: gx, y: gy }
}

/// Settle after raising the target app before the first event, so the window server has
/// finished the activation before input lands. 300ms, the value the validated probe
/// (`inject_input.swift`) used after `activate()`.
const FOCUS_SETTLE: Duration = Duration::from_millis(300);

/// Inject `event` (already window-relative PIXELS) as CGEvents targeting the app at `pid`,
/// mapping coordinates through `scale`/`origin_pt` (the active session's `pointPixelScale`
/// and window `contentRect.origin`, carried by `MacosPlatform` since the last `start_app` —
/// see `coords.rs`'s module doc).
pub(crate) fn send_pointer_by(
    event: &PointerEvent,
    pid: i32,
    scale: f64,
    origin_pt: (f64, f64),
    deadline: Deadline,
) -> Result<()> {
    // `Gesture` is unconditionally unsupported on macOS regardless of `pid`'s validity —
    // checked ahead of `focus` so it fails fast without raising/activating the app for an
    // operation that can never succeed, and so `focus`'s missing-process check can't mask
    // this call-shape error behind an unrelated `AppExited`.
    if matches!(event, PointerEvent::Gesture { .. }) {
        return Err(GlassError::unsupported(
            "multi_touch",
            crate::BACKEND,
            crate::MULTI_TOUCH.note,
        ));
    }
    focus_by(pid, deadline)?;
    require_payload_time(deadline)?;

    // Passing `None` here is a documented-valid `CGEventCreateMouseEvent`/
    // `CGEventCreateScrollWheelEvent2` argument (falls back to the combined session state),
    // so a `None` source (state-allocation failure — not observed in practice) degrades
    // gracefully rather than erroring the whole call.
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    let to_point = |x: i32, y: i32| to_cgpoint(x, y, scale, origin_pt);

    match *event {
        PointerEvent::Move { x, y } => {
            require_payload_time(deadline)?;
            let ev = mouse_event(
                source.as_deref(),
                CGEventType::MouseMoved,
                to_point(x, y),
                CGMouseButton::Left,
                CGEventFlags::empty(),
            )?;
            post(&ev);
            require_payload_time(deadline)?;
        }
        PointerEvent::Click {
            x,
            y,
            button,
            count,
            ref modifiers,
        } => {
            require_payload_time(deadline)?;
            let point = to_point(x, y);
            let flags = to_flags(modifiers);
            let cg_button = to_cg_button(button);
            let (down_ty, up_ty) = click_types(button);
            // One down/up pair stamped with `clicks` in `kCGMouseEventClickState`, rather
            // than `clicks` separate down/up pairs — the documented CGEvent technique for
            // synthesizing a double/triple click.
            let clicks = i64::from(count.max(1));
            let down = mouse_event(source.as_deref(), down_ty, point, cg_button, flags)?;
            CGEvent::set_integer_value_field(
                Some(&down),
                CGEventField::MouseEventClickState,
                clicks,
            );
            let up = mouse_event(source.as_deref(), up_ty, point, cg_button, flags)?;
            CGEvent::set_integer_value_field(Some(&up), CGEventField::MouseEventClickState, clicks);

            require_payload_time(deadline)?;
            post(&down);
            let down_deadline = require_payload_time(deadline);
            post(&up);
            down_deadline?;
            require_payload_time(deadline)?;
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
            let gesture = DragGesture::plan((from_x, from_y), (to_x, to_y), duration_ms);
            let mut sink = MacDragSink {
                source: source.as_deref(),
                scale,
                origin_pt,
                button,
                flags: to_flags(modifiers),
                last: CGPoint { x: 0.0, y: 0.0 },
            };
            after_focus(run_drag_input_by(&mut sink, &gesture, deadline))?;
        }
        PointerEvent::Scroll {
            x,
            y,
            dx,
            dy,
            ref modifiers,
        } => {
            let mut sink = MacScrollSink {
                source: source.as_deref(),
                point: to_point(x, y),
                dx,
                dy,
                flags: to_flags(modifiers),
                deadline,
            };
            after_focus(run_scroll_input_by(
                &mut sink,
                !modifiers.is_empty(),
                deadline,
            ))?;
        }
        // Already rejected by the early check above (before `focus`); kept here so this
        // match stays exhaustive over `PointerEvent`'s variants and a future variant added
        // to the enum still forces an explicit decision at this call site.
        PointerEvent::Gesture { .. } => {
            return Err(GlassError::unsupported(
                "multi_touch",
                crate::BACKEND,
                crate::capabilities().multi_touch.note,
            ));
        }
    }
    Ok(())
}

/// Inter-keystroke delay for `KeyEvent::Text` typing — matches `inject_input.swift`'s
/// `typeString`'s `usleep(12_000)`, so each keystroke is its own committed HID post before the
/// next one lands (see `glass_core::run_type`'s doc).
const KEY_TYPE_DWELL: Duration = Duration::from_millis(12);

/// Inject `event` as keyboard CGEvents targeting the app at `pid`, focusing it first (same
/// best-effort `focus` helper/settle `send_pointer` uses above).
pub(crate) fn send_key_by(event: &KeyEvent, pid: i32, deadline: Deadline) -> Result<()> {
    focus_by(pid, deadline)?;
    require_payload_time(deadline)?;

    // See `send_pointer`'s doc for why a `None` source is a documented-valid, gracefully
    // degrading `CGEventCreateKeyboardEvent` argument rather than an error.
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);

    match event {
        KeyEvent::Text(s) => {
            let mut sink = MacTypeSink {
                source: source.as_deref(),
            };
            after_focus(run_text_input_by(&mut sink, s, KEY_TYPE_DWELL, deadline))
        }
        KeyEvent::Chord(s) => send_chord_by(s, source.as_deref(), deadline),
    }
}

/// Build a keyboard CGEvent for `keycode` (down or up), tagged with `flags`. Mirrors
/// `mouse_event`'s shape — `CGEventCreateKeyboardEvent` failing (no observed cause; Apple
/// documents no failure mode beyond resource exhaustion) maps to the same
/// `GlassError::Backend` rather than a silent no-op post.
fn keyboard_event(
    source: Option<&CGEventSource>,
    keycode: u16,
    down: bool,
    flags: CGEventFlags,
) -> Result<CFRetained<CGEvent>> {
    let ev = CGEvent::new_keyboard_event(source, keycode, down)
        .ok_or_else(|| GlassError::Backend("CGEventCreateKeyboardEvent failed".into()))?;
    CGEvent::set_flags(Some(&ev), flags);
    Ok(ev)
}

/// Post one committed typed keystroke: a keyDown immediately followed by a keyUp, both carrying
/// `flags` such as Shift for an uppercase character.
fn tap_key(source: Option<&CGEventSource>, keycode: u16, flags: CGEventFlags) -> Result<()> {
    let down = keyboard_event(source, keycode, true, flags)?;
    post(&down);
    let up = keyboard_event(source, keycode, false, flags)?;
    post(&up);
    Ok(())
}

/// `TypeSink` for macOS: one keyDown+keyUp CGEvent pair per character. `CGEventPost` needs no
/// separate flush, so `run_type_by` starts its inter-character dwell (`KEY_TYPE_DWELL`) after
/// posting each pair. An unmappable char (no US-layout key — `keymap::key_for` returns `None`)
/// fails the whole call rather than silently skipping it, per the no-silent-fallback invariant.
struct MacTypeSink<'a> {
    source: Option<&'a CGEventSource>,
}

impl TypeSink for MacTypeSink<'_> {
    fn character(&mut self, c: char) -> Result<()> {
        let (keycode, shift) =
            keymap::key_for(c).ok_or_else(|| GlassError::InvalidKey(c.to_string()))?;
        let flags = if shift {
            CGEventFlags::MaskShift
        } else {
            CGEventFlags::empty()
        };
        tap_key(self.source, keycode, flags)
    }
}

/// Parse and post a chord like `"ctrl+shift+a"` or `"F4"`: every token but the last must be
/// a modifier `glass_core::keys::Modifier::from_name` recognizes (accumulated into
/// `CGEventFlags` via the same `to_flags` `send_pointer` uses); the last token is the key,
/// resolved via `resolve_chord_key`. `run_chord_by` sequences the keyDown+keyUp pair and its
/// deadline checks; the sink stamps the accumulated flags on both events because macOS needs no
/// separate native modifier events.
fn send_chord_by(chord: &str, source: Option<&CGEventSource>, deadline: Deadline) -> Result<()> {
    let parts: Vec<&str> = chord
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let Some((key_token, mod_tokens)) = parts.split_last() else {
        return Err(GlassError::InvalidKey(format!(
            "empty chord (no key token): '{chord}'"
        )));
    };

    let mut modifiers = Vec::with_capacity(mod_tokens.len());
    for m in mod_tokens {
        modifiers.push(Modifier::from_name(m).ok_or_else(|| {
            GlassError::InvalidKey(format!("unknown modifier '{m}' in '{chord}'"))
        })?);
    }
    let (keycode, needs_shift) = resolve_chord_key(key_token)
        .ok_or_else(|| GlassError::InvalidKey(format!("unknown key '{key_token}' in '{chord}'")))?;

    let mut flags = to_flags(&modifiers);
    if needs_shift {
        flags |= CGEventFlags::MaskShift;
    }
    let mut sink = MacChordSink {
        source,
        keycode,
        flags,
    };
    after_focus(run_chord_input_by(&mut sink, deadline))
}

struct MacChordSink<'a> {
    source: Option<&'a CGEventSource>,
    keycode: u16,
    flags: CGEventFlags,
}

impl ChordSink for MacChordSink<'_> {
    fn modifiers(&mut self, _down: bool) -> Result<()> {
        Ok(())
    }

    fn key(&mut self, down: bool) -> Result<()> {
        let event = keyboard_event(self.source, self.keycode, down, self.flags)?;
        post(&event);
        Ok(())
    }
}

/// Resolve a chord's final token to `(keycode, needs_shift)`: a single char goes through
/// [`keymap::key_for`] (which also reports whether that char needs Shift, e.g. `"ctrl+A"`);
/// anything else goes through [`keymap::keycode_for_keyname`] (a named key has no inherent
/// shift requirement of its own — any Shift comes from an explicit `shift` token in the
/// chord instead).
fn resolve_chord_key(token: &str) -> Option<(u16, bool)> {
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && let Some(mapped) = keymap::key_for(c)
    {
        return Some(mapped);
    }
    keymap::keycode_for_keyname(token).map(|code| (code, false))
}

/// Raise `pid`'s app before injecting input — macOS delivers CGEvents posted at the HID tap
/// to whatever the window server currently has focused, unlike X11/Windows where glass warps
/// the pointer into an app it already knows the geometry of.
///
/// A missing/exited process (`runningApplicationWithProcessIdentifier` returns `None`) is a
/// hard error, not best-effort: an agent driving a dead app should see
/// `GlassError::AppExited`, not a keystroke that silently landed somewhere else. A *declined*
/// activation (`activateWithOptions` returns `false`, e.g. the OS deprioritizes a background-app
/// request) does not fail the call — the process is confirmed alive and the event still posts.
///
/// `pub(crate)`: also the activation step of `backend::MacosPlatform::window`'s
/// `WindowOp::Focus` branch, ahead of its `axwindow::ax_raise`/`ax_set_main`.
pub(crate) fn focus(pid: i32) -> Result<()> {
    focus_by(pid, Deadline::UNBOUNDED)
}

pub(crate) fn focus_by(pid: i32, deadline: Deadline) -> Result<()> {
    if deadline.remaining().is_some_and(|left| left < FOCUS_SETTLE) {
        return Err(GlassError::deadline_not_started("macOS input focus"));
    }
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .ok_or(GlassError::AppExited(None))?;
    app.activateWithOptions(NSApplicationActivationOptions::empty());
    let settle = deadline
        .remaining()
        .unwrap_or(FOCUS_SETTLE)
        .min(FOCUS_SETTLE);
    thread::sleep(settle);
    require_payload_time(deadline)
}

/// Build a mouse CGEvent of `ty` at global `point`, tagged with `button`/`flags`. Does not
/// post it — callers that also need to stamp `kCGMouseEventClickState` (`Click`) do so
/// before posting.
fn mouse_event(
    source: Option<&CGEventSource>,
    ty: CGEventType,
    point: CGPoint,
    button: CGMouseButton,
    flags: CGEventFlags,
) -> Result<CFRetained<CGEvent>> {
    let ev = CGEvent::new_mouse_event(source, ty, point, button)
        .ok_or_else(|| GlassError::Backend("CGEventCreateMouseEvent failed".into()))?;
    CGEvent::set_flags(Some(&ev), flags);
    Ok(ev)
}

/// Post `ev` at the HID event tap — every posted event in this module goes through here.
fn post(ev: &CGEvent) {
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(ev));
}

/// Map `button` to the `CGMouseButton` `CGEventCreateMouseEvent` expects.
fn to_cg_button(button: MouseButton) -> CGMouseButton {
    match button {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Right => CGMouseButton::Right,
        MouseButton::Middle => CGMouseButton::Center,
    }
}

/// The (down, up) `CGEventType` pair for `button`.
fn click_types(button: MouseButton) -> (CGEventType, CGEventType) {
    match button {
        MouseButton::Left => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp),
        MouseButton::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp),
        MouseButton::Middle => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp),
    }
}

/// The `*MouseDragged` `CGEventType` for `button` — the motion type Apple expects while the
/// button is held (distinct from `MouseMoved`), so the target app's `mouseDragged:` handler
/// (not just `mouseMoved:`) fires during a drag.
fn dragged_type(button: MouseButton) -> CGEventType {
    match button {
        MouseButton::Left => CGEventType::LeftMouseDragged,
        MouseButton::Right => CGEventType::RightMouseDragged,
        MouseButton::Middle => CGEventType::OtherMouseDragged,
    }
}

/// Map glass's OS-agnostic modifiers to `CGEventFlags`. macOS conveys a held modifier via
/// flags set directly on each posted event, not a separate modifier-key down/up pair, so this
/// is the only modifier handling `send_pointer` needs — see [`MacDragSink::modifiers`]/
/// [`MacScrollSink::modifiers`]'s docs for why their trait hooks no-op.
fn to_flags(modifiers: &[Modifier]) -> CGEventFlags {
    modifiers.iter().fold(CGEventFlags::empty(), |acc, m| {
        acc | match m {
            Modifier::Shift => CGEventFlags::MaskShift,
            Modifier::Control => CGEventFlags::MaskControl,
            Modifier::Alt => CGEventFlags::MaskAlternate,
            Modifier::Super => CGEventFlags::MaskCommand,
        }
    })
}

/// Lets `glass_core::run_drag` drive a macOS drag through CGEvent. A CGEvent's `CGEventFlags`
/// are stamped directly on every posted event — unlike Windows' `SendInput` or X11's XTEST,
/// where a held modifier is a real key down/up — so `flags` is computed once from the gesture's
/// modifiers and applied to every `place`/`move_to`/`button` call; `modifiers()` emits nothing.
struct MacDragSink<'a> {
    source: Option<&'a CGEventSource>,
    scale: f64,
    origin_pt: (f64, f64),
    button: MouseButton,
    flags: CGEventFlags,
    /// Last point placed/moved to — `button()` posts here, since a mouse-button CGEvent's
    /// own `mouseCursorPosition` argument is authoritative (mirrors `glass-windows`'
    /// `WindowsDragSink::last`). `run_drag` always calls `place` before any `button`, so the
    /// `(0, 0)` seed is overwritten before it's ever read.
    last: CGPoint,
}

impl MacDragSink<'_> {
    fn to_point(&self, x: i32, y: i32) -> CGPoint {
        to_cgpoint(x, y, self.scale, self.origin_pt)
    }
}

impl DragSink for MacDragSink<'_> {
    fn place(&mut self, x: i32, y: i32) -> Result<()> {
        self.last = self.to_point(x, y);
        let ev = mouse_event(
            self.source,
            CGEventType::MouseMoved,
            self.last,
            to_cg_button(self.button),
            self.flags,
        )?;
        post(&ev);
        Ok(())
    }
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.last = self.to_point(x, y);
        let ev = mouse_event(
            self.source,
            dragged_type(self.button),
            self.last,
            to_cg_button(self.button),
            self.flags,
        )?;
        post(&ev);
        Ok(())
    }
    fn button(&mut self, down: bool) -> Result<()> {
        let (down_ty, up_ty) = click_types(self.button);
        let ev = mouse_event(
            self.source,
            if down { down_ty } else { up_ty },
            self.last,
            to_cg_button(self.button),
            self.flags,
        )?;
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventClickState, 1);
        post(&ev);
        Ok(())
    }
    fn modifiers(&mut self, _down: bool) -> Result<()> {
        // No-op: see this module's `to_flags` doc — the held modifiers are already baked
        // into every `place`/`move_to`/`button` event via `self.flags`.
        Ok(())
    }
}

/// Lets `glass_core::run_scroll` drive a macOS scroll through CGEvent — see
/// [`MacDragSink`]'s doc for why `modifiers()` no-ops here too.
struct MacScrollSink<'a> {
    source: Option<&'a CGEventSource>,
    point: CGPoint,
    dx: i32,
    dy: i32,
    flags: CGEventFlags,
    deadline: Deadline,
}

impl ScrollSink for MacScrollSink<'_> {
    fn modifiers(&mut self, _down: bool) -> Result<()> {
        Ok(())
    }
    fn wheel(&mut self) -> Result<()> {
        run_scroll_wheel_by(
            self.deadline,
            || {
                // `CGEventCreateScrollWheelEvent2` carries no target point of its own — the window
                // server delivers it to whatever's under the cursor (or the key window) — so
                // position the cursor first.
                let mv = mouse_event(
                    self.source,
                    CGEventType::MouseMoved,
                    self.point,
                    CGMouseButton::Left,
                    self.flags,
                )?;
                post(&mv);
                Ok(())
            },
            || {
                // Sign: verified against WebKit's macOS WebDriver wheel-action → CGEvent conversion
                // (`PlatformMac`'s `CGEventCreateScrollWheelEvent(..., -delta.height(),
                // -delta.width())`), which negates both axes converting a standard
                // positive-Y-is-down/positive-X-is-right delta into `wheel1`/`wheel2` — i.e. a
                // positive `wheel1`/`wheel2` scrolls up/left on macOS. That's the same
                // positive-Y-is-down/positive-X-is-right contract glass's own `dx`/`dy` already use
                // (see `glass-x11`'s `scroll_button(5=down,4=up, dy)`/`(7=right,6=left, dx)`), so the
                // same negation applies here: `wheel1 = -dy`, `wheel2 = -dx`.
                let ev = CGEvent::new_scroll_wheel_event2(
                    self.source,
                    CGScrollEventUnit::Line,
                    2,
                    -self.dy,
                    -self.dx,
                    0,
                )
                .ok_or_else(|| {
                    GlassError::Backend("CGEventCreateScrollWheelEvent2 failed".into())
                })?;
                CGEvent::set_flags(Some(&ev), self.flags);
                Ok(ev)
            },
            |ev| post(&ev),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_input_refuses_a_deadline_shorter_than_focus_settle_before_injection() {
        let err = focus_by(
            i32::MAX,
            Deadline::from_millis(FOCUS_SETTLE.as_millis() as u64 - 1),
        )
        .unwrap_err();
        assert!(matches!(err, GlassError::Bounded { .. }));
    }

    #[test]
    fn focus_that_spends_the_deadline_prevents_payload_dispatch() {
        let mut payload_events = Vec::new();
        let deadline = Deadline::at(std::time::Instant::now() - Duration::from_millis(1));

        let error = require_payload_time(deadline)
            .map(|()| {
                payload_events.push("payload");
            })
            .unwrap_err();

        assert!(payload_events.is_empty());
        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn text_deadline_is_checked_between_characters() {
        #[derive(Default)]
        struct SlowTypeSink {
            typed_characters: Vec<char>,
        }

        impl TypeSink for SlowTypeSink {
            fn character(&mut self, character: char) -> Result<()> {
                self.typed_characters.push(character);
                thread::sleep(Duration::from_millis(10));
                Ok(())
            }
        }

        let requested_text = "deadline";
        let mut sink = SlowTypeSink::default();

        let error = run_text_input_by(
            &mut sink,
            requested_text,
            Duration::ZERO,
            Deadline::from_millis(5),
        )
        .unwrap_err();

        assert!(sink.typed_characters.len() < requested_text.chars().count());
        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
    }

    #[test]
    fn drag_and_scroll_cap_driver_sleeps_by_remaining_time() {
        #[derive(Default)]
        struct NoopDragSink;

        impl DragSink for NoopDragSink {
            fn place(&mut self, _x: i32, _y: i32) -> Result<()> {
                Ok(())
            }

            fn move_to(&mut self, _x: i32, _y: i32) -> Result<()> {
                Ok(())
            }

            fn button(&mut self, _down: bool) -> Result<()> {
                Ok(())
            }

            fn modifiers(&mut self, _down: bool) -> Result<()> {
                Ok(())
            }
        }

        #[derive(Default)]
        struct NoopScrollSink;

        impl ScrollSink for NoopScrollSink {
            fn modifiers(&mut self, _down: bool) -> Result<()> {
                Ok(())
            }

            fn wheel(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let caller_remaining = Duration::from_millis(100);
        let gesture = DragGesture {
            waypoints: vec![(0, 0), (1, 1)],
            step: Duration::from_millis(200),
            dwell: Duration::from_millis(200),
        };
        let mut drag_sink = NoopDragSink;
        let started = std::time::Instant::now();
        let drag_error =
            run_drag_input_by(&mut drag_sink, &gesture, Deadline::from_millis(10)).unwrap_err();
        let observed_sleep = started.elapsed();

        assert!(observed_sleep <= caller_remaining);
        assert_eq!(drag_error.bound(), Some(glass_core::BoundKind::TimedOut));

        let mut scroll_sink = NoopScrollSink;
        let started = std::time::Instant::now();
        let scroll_error =
            run_scroll_input_by(&mut scroll_sink, true, Deadline::from_millis(10)).unwrap_err();
        let observed_sleep = started.elapsed();

        assert!(observed_sleep <= caller_remaining);
        assert_eq!(scroll_error.bound(), Some(glass_core::BoundKind::TimedOut));
    }

    #[test]
    fn chord_deadline_caps_the_driver_dwell_before_the_key_event() {
        #[derive(Default)]
        struct RecordingChordSink {
            key_events: Vec<bool>,
        }

        impl glass_core::ChordSink for RecordingChordSink {
            fn modifiers(&mut self, _down: bool) -> Result<()> {
                Ok(())
            }

            fn key(&mut self, down: bool) -> Result<()> {
                self.key_events.push(down);
                Ok(())
            }
        }

        let mut sink = RecordingChordSink::default();

        let error = run_chord_input_by(&mut sink, Deadline::from_millis(10)).unwrap_err();

        assert!(sink.key_events.is_empty());
        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
    }

    #[test]
    fn to_flags_maps_each_modifier_and_combines() {
        assert_eq!(to_flags(&[]), CGEventFlags::empty());
        assert_eq!(to_flags(&[Modifier::Shift]), CGEventFlags::MaskShift);
        assert_eq!(to_flags(&[Modifier::Control]), CGEventFlags::MaskControl);
        assert_eq!(to_flags(&[Modifier::Alt]), CGEventFlags::MaskAlternate);
        assert_eq!(to_flags(&[Modifier::Super]), CGEventFlags::MaskCommand);
        assert_eq!(
            to_flags(&[Modifier::Control, Modifier::Super]),
            CGEventFlags::MaskControl | CGEventFlags::MaskCommand
        );
    }

    #[test]
    fn button_type_mappings_cover_all_three_buttons() {
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            let (down, up) = click_types(button);
            // Every button's down/up/dragged types are pairwise distinct — catches a
            // copy-paste that mapped two buttons to the same CGEventType.
            assert_ne!(down, up);
            assert_ne!(dragged_type(button), down);
            assert_ne!(dragged_type(button), up);
        }
        assert_eq!(to_cg_button(MouseButton::Left), CGMouseButton::Left);
        assert_eq!(to_cg_button(MouseButton::Right), CGMouseButton::Right);
        assert_eq!(to_cg_button(MouseButton::Middle), CGMouseButton::Center);
        assert_eq!(
            click_types(MouseButton::Left),
            (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp)
        );
        assert_eq!(
            click_types(MouseButton::Right),
            (CGEventType::RightMouseDown, CGEventType::RightMouseUp)
        );
        assert_eq!(
            click_types(MouseButton::Middle),
            (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp)
        );
        assert_eq!(
            dragged_type(MouseButton::Left),
            CGEventType::LeftMouseDragged
        );
        assert_eq!(
            dragged_type(MouseButton::Right),
            CGEventType::RightMouseDragged
        );
        assert_eq!(
            dragged_type(MouseButton::Middle),
            CGEventType::OtherMouseDragged
        );
    }

    #[test]
    fn gesture_is_unsupported() {
        let err = send_pointer_by(
            &PointerEvent::Gesture {
                pointers: vec![],
                duration_ms: 0,
            },
            1,
            1.0,
            (0.0, 0.0),
            Deadline::UNBOUNDED,
        );
        assert!(
            matches!(&err, Err(GlassError::Unsupported(_))),
            "expected Unsupported, got {err:?}"
        );
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("macos backend"), "{msg}");
        assert!(msg.contains("multi_touch"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
    }

    #[test]
    fn resolve_chord_key_single_char_uses_key_for_and_reports_shift() {
        // Lowercase needs no Shift; uppercase does — same physical key either way, matching
        // `keymap::key_for`'s own contract.
        assert_eq!(resolve_chord_key("a"), Some((0, false)));
        assert_eq!(resolve_chord_key("A"), Some((0, true)));
    }

    #[test]
    fn resolve_chord_key_named_key_has_no_inherent_shift() {
        assert_eq!(resolve_chord_key("Return"), Some((36, false)));
        assert_eq!(resolve_chord_key("F4"), Some((118, false)));
    }

    #[test]
    fn resolve_chord_key_unknown_is_none() {
        assert_eq!(resolve_chord_key("nope"), None);
    }

    #[test]
    fn mac_type_sink_rejects_unmappable_char() {
        // Errors before posting anything, so a `None` source is safe here too.
        let mut sink = MacTypeSink { source: None };
        assert!(matches!(
            sink.character('€'),
            Err(GlassError::InvalidKey(_))
        ));
    }

    #[test]
    fn send_chord_rejects_empty_unknown_modifier_and_unknown_key() {
        // None of these reach `tap_key` (they error before posting anything), so a `None`
        // event source is safe to pass here. Each also asserts the message names the
        // specific bad token — mirroring `glass_core::keys::parse_chord`'s specificity —
        // so a regression that flattens these back to a bare `chord.to_string()` is caught.
        match send_chord_by("", None, Deadline::UNBOUNDED) {
            Err(GlassError::InvalidKey(msg)) => {
                assert!(
                    msg.contains("empty"),
                    "expected an 'empty chord' message, got {msg:?}"
                );
            }
            other => panic!("expected InvalidKey, got {other:?}"),
        }
        match send_chord_by("hyper+x", None, Deadline::UNBOUNDED) {
            Err(GlassError::InvalidKey(msg)) => {
                assert!(
                    msg.contains("hyper"),
                    "message should name the bad modifier, got {msg:?}"
                );
            }
            other => panic!("expected InvalidKey, got {other:?}"),
        }
        match send_chord_by("ctrl+nope", None, Deadline::UNBOUNDED) {
            Err(GlassError::InvalidKey(msg)) => {
                assert!(
                    msg.contains("nope"),
                    "message should name the bad key, got {msg:?}"
                );
            }
            other => panic!("expected InvalidKey, got {other:?}"),
        }
    }
}
