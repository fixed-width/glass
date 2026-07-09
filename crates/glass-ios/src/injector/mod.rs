pub mod keymap;

use glass_core::{GlassError, PointerEvent, Result};

use crate::idb::proto;

/// Pixels of swipe travel per scroll "click" (matches glass-android's tunable).
const SCROLL_STEP_PX: i32 = 120;
/// Default swipe duration (seconds) for drags that don't specify one, and scrolls.
const SWIPE_SECS: f64 = 0.3;

// A later increment wires `IosPlatform::pointer` to this (Task 9); until then nothing
// in-crate calls it beyond its own tests, and the `injector` module is crate-private,
// so `pub` alone does not exempt it from `dead_code`.
/// Builds idb `HIDEvent`s from glass input. `scale` converts window-relative
/// pixels (glass's coordinate space, matching the capture `Frame`) to the logical
/// points idb expects: `point = pixel / scale`.
#[allow(dead_code)]
pub struct IdbInjector {
    scale: f64,
}

fn point(x_px: i32, y_px: i32, scale: f64) -> proto::Point {
    proto::Point {
        x: x_px as f64 / scale,
        y: y_px as f64 / scale,
    }
}

fn touch(pt: proto::Point, down: bool) -> proto::HidEvent {
    use proto::hid_event::{
        hid_press_action::Action, Event, HidDirection, HidPress, HidPressAction, HidTouch,
    };
    proto::HidEvent {
        event: Some(Event::Press(HidPress {
            action: Some(HidPressAction {
                action: Some(Action::Touch(HidTouch { point: Some(pt) })),
            }),
            direction: if down {
                HidDirection::Down as i32
            } else {
                HidDirection::Up as i32
            },
        })),
    }
}

fn swipe(from: proto::Point, to: proto::Point, secs: f64) -> proto::HidEvent {
    use proto::hid_event::{Event, HidSwipe};
    proto::HidEvent {
        event: Some(Event::Swipe(HidSwipe {
            start: Some(from),
            end: Some(to),
            delta: 0.0,
            duration: secs,
        })),
    }
}

#[allow(dead_code)]
impl IdbInjector {
    pub fn new(scale: f64) -> Self {
        IdbInjector { scale }
    }

    /// Maps one glass `PointerEvent` to the idb HID events that reproduce it as a
    /// touch: a `Click` is a touch DOWN then UP at the same point (repeated `count`
    /// times); a `Drag`/`Scroll` is a single swipe; a `Move` is empty — touch has no
    /// hover state to emit. `Gesture` (multi-touch) is not implemented yet and
    /// returns `Unsupported` rather than silently dropping the pointers.
    pub fn pointer_events(&self, e: &PointerEvent) -> Result<Vec<proto::HidEvent>> {
        let s = self.scale;
        Ok(match *e {
            PointerEvent::Move { .. } => vec![], // touch has no hover
            PointerEvent::Click { x, y, count, .. } => {
                let mut v = Vec::new();
                for _ in 0..count.max(1) {
                    v.push(touch(point(x, y, s), true));
                    v.push(touch(point(x, y, s), false));
                }
                v
            }
            PointerEvent::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                duration_ms,
                ..
            } => {
                let secs = if duration_ms == 0 {
                    SWIPE_SECS
                } else {
                    duration_ms as f64 / 1000.0
                };
                vec![swipe(point(from_x, from_y, s), point(to_x, to_y, s), secs)]
            }
            PointerEvent::Scroll { x, y, dx, dy, .. } => {
                // Touch scroll = swipe opposite the wheel direction, anchored at the point.
                let ex = x - dx * SCROLL_STEP_PX;
                let ey = y - dy * SCROLL_STEP_PX;
                vec![swipe(point(x, y, s), point(ex, ey, s), SWIPE_SECS)]
            }
            PointerEvent::Gesture { .. } => {
                return Err(GlassError::Unsupported(
                    "multi-touch gestures are not supported by the iOS backend yet".into(),
                ))
            }
        })
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::*;
    use glass_core::{MouseButton, PointerEvent};

    fn touch_points(evts: &[proto::HidEvent]) -> Vec<(f64, f64)> {
        // Extract (x,y) from each press-touch event, in order.
        evts.iter()
            .filter_map(|e| match &e.event {
                Some(proto::hid_event::Event::Press(p)) => match &p.action {
                    Some(a) => match &a.action {
                        Some(proto::hid_event::hid_press_action::Action::Touch(t)) => {
                            t.point.as_ref().map(|pt| (pt.x, pt.y))
                        }
                        _ => None,
                    },
                    None => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn click_is_touch_down_up_in_points() {
        let inj = IdbInjector::new(3.0);
        let e = PointerEvent::Click {
            x: 300,
            y: 600,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        };
        let evts = inj.pointer_events(&e).unwrap();
        // One down + one up at the same point, px/scale = 100,200.
        assert_eq!(touch_points(&evts), vec![(100.0, 200.0), (100.0, 200.0)]);
    }

    #[test]
    fn move_is_empty_and_gesture_unsupported() {
        let inj = IdbInjector::new(3.0);
        assert!(inj
            .pointer_events(&PointerEvent::Move { x: 1, y: 2 })
            .unwrap()
            .is_empty());
        let g = PointerEvent::Gesture {
            pointers: vec![],
            duration_ms: 100,
        };
        assert!(matches!(
            inj.pointer_events(&g),
            Err(glass_core::GlassError::Unsupported(_))
        ));
    }

    #[test]
    fn drag_emits_one_swipe() {
        let inj = IdbInjector::new(3.0);
        let e = PointerEvent::Drag {
            from_x: 30,
            from_y: 60,
            to_x: 300,
            to_y: 600,
            button: MouseButton::Left,
            modifiers: vec![],
            duration_ms: 250,
        };
        let evts = inj.pointer_events(&e).unwrap();
        let swipes: Vec<_> = evts
            .iter()
            .filter(|e| matches!(&e.event, Some(proto::hid_event::Event::Swipe(_))))
            .collect();
        assert_eq!(swipes.len(), 1);
    }
}
