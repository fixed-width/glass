//! Pure builders that turn glass input events into `adb shell input …` command
//! argument vectors, plus the `Injector` seam (`ShellInjector` shells out; a
//! future on-device agent can replace it for lower latency).

use glass_core::{PointerEvent, WindowGeometry};

/// Pixels of swipe travel per scroll "click" (`Scroll.dx/dy` are wheel clicks —
/// X11 clicks the wheel `|delta|` times). Tunable.
const SCROLL_STEP_PX: i32 = 120;
/// Swipe duration (ms) for scrolls, and for drags that don't specify one.
const SWIPE_MS: u64 = 300;

/// Build the `adb shell input …` command(s) for a pointer event, mapping
/// window-relative coords to absolute device coords via `origin`. One argv per
/// command; an empty vec means "nothing to inject" (a touch `Move` has no hover
/// equivalent). Mouse button and keyboard modifiers are ignored — a touch
/// contact is single-button and can't carry modifiers.
pub fn pointer_commands(origin: &WindowGeometry, event: &PointerEvent) -> Vec<Vec<String>> {
    let abs = |x: i32, y: i32| (origin.x + x, origin.y + y);
    match *event {
        PointerEvent::Move { .. } => vec![],
        PointerEvent::Click { x, y, count, .. } => {
            let (ax, ay) = abs(x, y);
            (0..count.max(1)).map(|_| tap(ax, ay)).collect()
        }
        PointerEvent::Drag { from_x, from_y, to_x, to_y, duration_ms, .. } => {
            let (fx, fy) = abs(from_x, from_y);
            let (tx, ty) = abs(to_x, to_y);
            let ms = if duration_ms == 0 { SWIPE_MS } else { duration_ms };
            vec![swipe(fx, fy, tx, ty, ms)]
        }
        PointerEvent::Scroll { x, y, dx, dy, .. } => {
            let (cx, cy) = abs(x, y);
            // Touch scroll = swipe opposite the wheel direction.
            let hi_x = (origin.x + origin.width as i32 - 1).max(origin.x);
            let hi_y = (origin.y + origin.height as i32 - 1).max(origin.y);
            let ex = cx.saturating_sub(dx.saturating_mul(SCROLL_STEP_PX)).clamp(origin.x, hi_x);
            let ey = cy.saturating_sub(dy.saturating_mul(SCROLL_STEP_PX)).clamp(origin.y, hi_y);
            vec![swipe(cx, cy, ex, ey, SWIPE_MS)]
        }
    }
}

fn tap(x: i32, y: i32) -> Vec<String> {
    vec!["shell".into(), "input".into(), "tap".into(), x.to_string(), y.to_string()]
}

fn swipe(x1: i32, y1: i32, x2: i32, y2: i32, ms: u64) -> Vec<String> {
    vec![
        "shell".into(), "input".into(), "swipe".into(),
        x1.to_string(), y1.to_string(), x2.to_string(), y2.to_string(), ms.to_string(),
    ]
}

#[cfg(test)]
mod pointer_tests {
    use super::*;
    use glass_core::{MouseButton, PointerEvent, WindowGeometry};

    fn win() -> WindowGeometry {
        WindowGeometry { x: 0, y: 63, width: 1080, height: 2400 }
    }

    #[test]
    fn move_injects_nothing() {
        assert!(pointer_commands(&win(), &PointerEvent::Move { x: 5, y: 5 }).is_empty());
    }

    #[test]
    fn click_taps_at_absolute_coords() {
        let ev = PointerEvent::Click {
            x: 10, y: 20, button: MouseButton::Left, count: 1, modifiers: vec![],
        };
        assert_eq!(pointer_commands(&win(), &ev), vec![vec![
            "shell".to_string(), "input".into(), "tap".into(), "10".into(), "83".into(),
        ]]);
    }

    #[test]
    fn multi_count_click_taps_repeatedly() {
        let ev = PointerEvent::Click {
            x: 1, y: 1, button: MouseButton::Left, count: 2, modifiers: vec![],
        };
        assert_eq!(pointer_commands(&win(), &ev).len(), 2);
    }

    #[test]
    fn drag_swipes_with_duration() {
        let ev = PointerEvent::Drag {
            from_x: 0, from_y: 0, to_x: 100, to_y: 200,
            button: MouseButton::Left, modifiers: vec![], duration_ms: 250,
        };
        assert_eq!(pointer_commands(&win(), &ev), vec![vec![
            "shell".to_string(), "input".into(), "swipe".into(),
            "0".into(), "63".into(), "100".into(), "263".into(), "250".into(),
        ]]);
    }

    #[test]
    fn scroll_down_swipes_upward_opposite_the_wheel() {
        let ev = PointerEvent::Scroll { x: 540, y: 1200, dx: 0, dy: 1, modifiers: vec![] };
        let got = pointer_commands(&win(), &ev);
        assert_eq!(got, vec![vec![
            "shell".to_string(), "input".into(), "swipe".into(),
            "540".into(), "1263".into(), "540".into(), "1143".into(), "300".into(),
        ]]);
    }

    #[test]
    fn scroll_clamps_to_the_window() {
        let ev = PointerEvent::Scroll { x: 10, y: 1, dx: 0, dy: 100, modifiers: vec![] };
        let got = pointer_commands(&win(), &ev);
        let end_y = &got[0][6];
        assert_eq!(end_y, "63");
    }
}
