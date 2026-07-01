//! CGEvent mouse injection: `send_pointer` maps each window-relative PIXEL coordinate to a
//! global Quartz POINT via `coords::pixel_to_global_point`, raises the target app
//! (focus-before-inject), then posts `Move`/`Click`/`Drag`/`Scroll` as CGEvents through the
//! HID event tap. `Gesture` (Android multi-touch) is `Unsupported` — see
//! `glass_core::platform::PointerEvent`'s doc.
//!
//! Ported from the proven reference `tools/macos-validation/inject_input.swift`'s
//! `clickGlobal` (mouse down/up via `CGEvent(mouseEventSource:mouseType:...)` +
//! `.post(tap: .cghidEventTap)`) and its focus-before-inject
//! (`NSRunningApplication(processIdentifier:).activate()`), onto `objc2-core-graphics`'s
//! generated bindings — which expose `CGEvent`'s constructors/accessors as **associated**
//! functions taking `Option<&CGEvent>` (e.g. `CGEvent::post(tap, event)`,
//! `CGEvent::set_flags(event, flags)`), not Swift's `self`-methods. header-translator marks
//! all of them (and `NSRunningApplication::activateWithOptions`) as plain safe Rust
//! functions — their only precondition, a live `CGEvent`/`CGEventSource`/`NSRunningApplication`
//! reference, is already enforced by the type system — so no `unsafe` block is needed
//! anywhere in this file.
//!
//! Drag/scroll reuse glass_core's shared, already-unit-tested drivers
//! ([`glass_core::run_drag`]/[`glass_core::DragGesture`], [`glass_core::run_scroll`]) — the
//! same ones `glass-windows`/`glass-x11` sequence through their own `DragSink`/`ScrollSink` —
//! so the waypoint interpolation/pacing/dwell math isn't reimplemented here.
//!
//! **Main-thread affinity:** like `backend.rs`'s `start_app`/`capture_frame`, this is called
//! from `MacosPlatform::send_pointer`, which glass always drives from the main thread; wiring
//! that under glass-mcp's worker-thread dispatcher is deferred to Plan 5 (see `backend.rs`'s
//! module doc).

use std::thread;
use std::time::Duration;

use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton, CGScrollEventUnit,
};

use glass_core::keys::Modifier;
use glass_core::platform::{MouseButton, PointerEvent};
use glass_core::{run_drag, run_scroll, DragGesture, DragSink, GlassError, Result, ScrollSink};

use crate::coords::pixel_to_global_point;

/// Settle after raising the target app before the first event, so the window server has
/// finished the activation before input lands. The validated probe (`inject_input.swift`)
/// used 300ms after `activate()`; glass's own activation call is otherwise identical, so this
/// clears the same race with margin.
const FOCUS_SETTLE: Duration = Duration::from_millis(150);

/// Inject `event` (already window-relative PIXELS) as CGEvents targeting the app at `pid`,
/// mapping coordinates through `scale`/`origin_pt` (the active session's `pointPixelScale`
/// and window `contentRect.origin`, carried by `MacosPlatform` since the last `start_app` —
/// see `coords.rs`'s module doc).
pub(crate) fn send_pointer(event: &PointerEvent, pid: i32, scale: f64, origin_pt: (f64, f64)) -> Result<()> {
    focus(pid);

    // Passing `None` here is a documented-valid `CGEventCreateMouseEvent`/
    // `CGEventCreateScrollWheelEvent2` argument (falls back to the combined session state),
    // so a `None` source (state-allocation failure — not observed in practice) degrades
    // gracefully rather than erroring the whole call.
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    let to_point = |x: i32, y: i32| -> CGPoint {
        let (gx, gy) = pixel_to_global_point((x, y), scale, origin_pt);
        CGPoint { x: gx, y: gy }
    };

    match *event {
        PointerEvent::Move { x, y } => {
            let ev = mouse_event(
                source.as_deref(),
                CGEventType::MouseMoved,
                to_point(x, y),
                CGMouseButton::Left,
                CGEventFlags::empty(),
            )?;
            post(&ev);
        }
        PointerEvent::Click { x, y, button, count, ref modifiers } => {
            let point = to_point(x, y);
            let flags = to_flags(modifiers);
            let cg_button = to_cg_button(button);
            let (down_ty, up_ty) = click_types(button);
            // One down/up pair stamped with `clicks` in `kCGMouseEventClickState`, rather
            // than `clicks` separate down/up pairs — the documented CGEvent technique for
            // synthesizing a double/triple click (see Task 2's brief).
            let clicks = i64::from(count.max(1));
            let down = mouse_event(source.as_deref(), down_ty, point, cg_button, flags)?;
            CGEvent::set_integer_value_field(Some(&down), CGEventField::MouseEventClickState, clicks);
            post(&down);
            let up = mouse_event(source.as_deref(), up_ty, point, cg_button, flags)?;
            CGEvent::set_integer_value_field(Some(&up), CGEventField::MouseEventClickState, clicks);
            post(&up);
        }
        PointerEvent::Drag { from_x, from_y, to_x, to_y, button, ref modifiers, duration_ms } => {
            let gesture = DragGesture::plan((from_x, from_y), (to_x, to_y), duration_ms);
            let mut sink = MacDragSink {
                source: source.as_deref(),
                scale,
                origin_pt,
                button,
                flags: to_flags(modifiers),
                last: CGPoint { x: 0.0, y: 0.0 },
            };
            run_drag(&mut sink, &gesture)?;
        }
        PointerEvent::Scroll { x, y, dx, dy, ref modifiers } => {
            let mut sink =
                MacScrollSink { source: source.as_deref(), point: to_point(x, y), dx, dy, flags: to_flags(modifiers) };
            run_scroll(&mut sink, !modifiers.is_empty())?;
        }
        PointerEvent::Gesture { .. } => {
            return Err(GlassError::Unsupported("multi-touch gesture is not supported on macOS".into()));
        }
    }
    Ok(())
}

/// Raise `pid`'s app before injecting input — macOS delivers CGEvents posted at the HID tap
/// to whatever the window server currently has focused, unlike X11/Windows where glass warps
/// the pointer into an app it already knows the geometry of. Best-effort: a missing/exited
/// app (`runningApplicationWithProcessIdentifier` returns `None`) or a declined activation
/// (`activateWithOptions` returns `false`) doesn't fail the call — the event still posts to
/// whatever currently has focus, matching `glass-windows::input::send_pointer`'s own
/// best-effort `focus_window` nudge.
fn focus(pid: i32) {
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        return;
    };
    app.activateWithOptions(NSApplicationActivationOptions::empty());
    thread::sleep(FOCUS_SETTLE);
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

/// Lets `glass_core::run_drag` drive a macOS drag through CGEvent. Unlike Windows'
/// `SendInput` (no per-event flags field, so held modifiers are real key down/up events) or
/// X11 (XTEST key press/release), a CGEvent's `CGEventFlags` are stamped directly on every
/// posted event — so `flags` is computed once from the gesture's modifiers and applied to
/// every `place`/`move_to`/`button` call; `modifiers()` itself has nothing to emit.
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
        let (gx, gy) = pixel_to_global_point((x, y), self.scale, self.origin_pt);
        CGPoint { x: gx, y: gy }
    }
}

impl DragSink for MacDragSink<'_> {
    fn place(&mut self, x: i32, y: i32) -> Result<()> {
        self.last = self.to_point(x, y);
        let ev = mouse_event(self.source, CGEventType::MouseMoved, self.last, to_cg_button(self.button), self.flags)?;
        post(&ev);
        Ok(())
    }
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.last = self.to_point(x, y);
        let ev =
            mouse_event(self.source, dragged_type(self.button), self.last, to_cg_button(self.button), self.flags)?;
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
}

impl ScrollSink for MacScrollSink<'_> {
    fn modifiers(&mut self, _down: bool) -> Result<()> {
        Ok(())
    }
    fn wheel(&mut self) -> Result<()> {
        // `CGEventCreateScrollWheelEvent2` carries no target point of its own — the window
        // server delivers it to whatever's under the cursor (or the key window) — so
        // position the cursor first (Task 2's brief).
        let mv = mouse_event(self.source, CGEventType::MouseMoved, self.point, CGMouseButton::Left, self.flags)?;
        post(&mv);

        // Sign: verified against WebKit's macOS WebDriver wheel-action → CGEvent conversion
        // (`PlatformMac`'s `CGEventCreateScrollWheelEvent(..., -delta.height(),
        // -delta.width())`), which negates both axes converting a standard
        // positive-Y-is-down/positive-X-is-right delta into `wheel1`/`wheel2` — i.e. a
        // positive `wheel1`/`wheel2` scrolls up/left on macOS. That's the same
        // positive-Y-is-down/positive-X-is-right contract glass's own `dx`/`dy` already use
        // (see `glass-x11`'s `scroll_button(5=down,4=up, dy)`/`(7=right,6=left, dx)`), so the
        // same negation applies here: `wheel1 = -dy`, `wheel2 = -dx`.
        let ev = CGEvent::new_scroll_wheel_event2(self.source, CGScrollEventUnit::Line, 2, -self.dy, -self.dx, 0)
            .ok_or_else(|| GlassError::Backend("CGEventCreateScrollWheelEvent2 failed".into()))?;
        CGEvent::set_flags(Some(&ev), self.flags);
        post(&ev);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(click_types(MouseButton::Left), (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp));
        assert_eq!(click_types(MouseButton::Right), (CGEventType::RightMouseDown, CGEventType::RightMouseUp));
        assert_eq!(click_types(MouseButton::Middle), (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp));
        assert_eq!(dragged_type(MouseButton::Left), CGEventType::LeftMouseDragged);
        assert_eq!(dragged_type(MouseButton::Right), CGEventType::RightMouseDragged);
        assert_eq!(dragged_type(MouseButton::Middle), CGEventType::OtherMouseDragged);
    }

    #[test]
    fn gesture_is_unsupported() {
        let err = send_pointer(&PointerEvent::Gesture { pointers: vec![], duration_ms: 0 }, 1, 1.0, (0.0, 0.0));
        assert!(matches!(&err, Err(GlassError::Unsupported(_))), "expected Unsupported, got {err:?}");
    }
}
