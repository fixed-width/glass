pub mod keymap;

use glass_core::{GlassError, KeyEvent, Modifier, PointerEvent, Result};

use crate::idb::proto;
use keymap::{char_usage, keyname_usage, modifier_usage};

/// Pixels of swipe travel per scroll "click" (matches glass-android's tunable).
const SCROLL_STEP_PX: i32 = 120;
/// Default swipe duration (seconds) for drags that don't specify one, and scrolls.
const SWIPE_SECS: f64 = 0.3;

/// Builds idb `HIDEvent`s from glass input. `scale` converts window-relative
/// pixels (glass's coordinate space, matching the capture `Frame`) to the logical
/// points idb expects: `point = pixel / scale`.
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

fn key(code: u16, down: bool) -> proto::HidEvent {
    use proto::hid_event::{
        hid_press_action::Action, Event, HidDirection, HidKey, HidPress, HidPressAction,
    };
    proto::HidEvent {
        event: Some(Event::Press(HidPress {
            action: Some(HidPressAction {
                action: Some(Action::Key(HidKey {
                    keycode: code as u64,
                })),
            }),
            direction: if down {
                HidDirection::Down as i32
            } else {
                HidDirection::Up as i32
            },
        })),
    }
}

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

    /// Maps one glass `KeyEvent` to the idb HID key events that reproduce it. `Text`
    /// types each char in turn: a Shift down/up bracketing the key down/up when the
    /// char needs it, nothing otherwise. `Chord` (e.g. `"ctrl+shift+a"`) holds every
    /// modifier down, presses the final key, then releases the modifiers in reverse
    /// order. An unmappable text char is `Unsupported`; an unknown chord key or
    /// modifier name is `InvalidKey` — neither is dropped silently.
    pub fn key_events(&self, e: &KeyEvent) -> Result<Vec<proto::HidEvent>> {
        match e {
            KeyEvent::Text(s) => {
                let mut v = Vec::new();
                for c in s.chars() {
                    let (usage, shift) = char_usage(c).ok_or_else(|| {
                        GlassError::Unsupported(format!(
                            "cannot type {c:?} on the iOS backend (US-ASCII only)"
                        ))
                    })?;
                    if shift {
                        v.push(key(modifier_usage(Modifier::Shift), true));
                    }
                    v.push(key(usage, true));
                    v.push(key(usage, false));
                    if shift {
                        v.push(key(modifier_usage(Modifier::Shift), false));
                    }
                }
                Ok(v)
            }
            KeyEvent::Chord(chord) => {
                let (mods, key_name) = split_chord(chord)?;
                let usage = keyname_usage(&key_name).ok_or_else(|| {
                    GlassError::InvalidKey(format!("unknown key {key_name:?} in {chord:?}"))
                })?;
                let mut v = Vec::new();
                for m in &mods {
                    v.push(key(modifier_usage(*m), true));
                }
                v.push(key(usage, true));
                v.push(key(usage, false));
                for m in mods.iter().rev() {
                    v.push(key(modifier_usage(*m), false));
                }
                Ok(v)
            }
        }
    }
}

/// Splits a chord like `"ctrl+shift+a"` into its modifiers and final key name.
/// Modifier tokens use glass-core's `Modifier` vocabulary; the final key is looked
/// up with `keyname_usage`.
fn split_chord(chord: &str) -> Result<(Vec<Modifier>, String)> {
    let parts: Vec<&str> = chord
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let (key_name, mods) = parts
        .split_last()
        .ok_or_else(|| GlassError::InvalidKey(format!("empty chord {chord:?}")))?;
    let mut modifiers = Vec::new();
    for m in mods {
        modifiers.push(Modifier::from_name(m).ok_or_else(|| {
            GlassError::InvalidKey(format!(
                "unknown modifier {m:?} in {chord:?} (use ctrl/shift/alt/super/cmd)"
            ))
        })?);
    }
    Ok((modifiers, key_name.to_string()))
}

#[cfg(test)]
mod pointer_tests {
    use super::*;
    use glass_core::{MouseButton, PointerEvent};
    use proto::hid_event::{HidDirection, HidSwipe};

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

    fn touch_directions(evts: &[proto::HidEvent]) -> Vec<i32> {
        // Extract the DOWN/UP direction of each press-touch event, in order.
        evts.iter()
            .filter_map(|e| match &e.event {
                Some(proto::hid_event::Event::Press(p)) => match &p.action {
                    Some(a) => match &a.action {
                        Some(proto::hid_event::hid_press_action::Action::Touch(_)) => {
                            Some(p.direction)
                        }
                        _ => None,
                    },
                    None => None,
                },
                _ => None,
            })
            .collect()
    }

    fn swipes(evts: &[proto::HidEvent]) -> Vec<&HidSwipe> {
        evts.iter()
            .filter_map(|e| match &e.event {
                Some(proto::hid_event::Event::Swipe(s)) => Some(s),
                _ => None,
            })
            .collect()
    }

    fn swipe_ends(s: &HidSwipe) -> ((f64, f64), (f64, f64)) {
        let start = s.start.as_ref().map(|p| (p.x, p.y)).unwrap();
        let end = s.end.as_ref().map(|p| (p.x, p.y)).unwrap();
        (start, end)
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
        // ...and in DOWN-then-UP order (points alone wouldn't catch a swap).
        assert_eq!(
            touch_directions(&evts),
            vec![HidDirection::Down as i32, HidDirection::Up as i32]
        );
    }

    #[test]
    fn click_count_repeats_down_up_per_count() {
        let inj = IdbInjector::new(3.0);
        let e = PointerEvent::Click {
            x: 300,
            y: 600,
            button: MouseButton::Left,
            count: 3,
            modifiers: vec![],
        };
        let evts = inj.pointer_events(&e).unwrap();
        // Each count yields a down + an up: 2 * count touch events.
        assert_eq!(touch_points(&evts).len(), 6);
        assert_eq!(
            touch_directions(&evts),
            vec![
                HidDirection::Down as i32,
                HidDirection::Up as i32,
                HidDirection::Down as i32,
                HidDirection::Up as i32,
                HidDirection::Down as i32,
                HidDirection::Up as i32,
            ]
        );
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
        let swipes = swipes(&evts);
        assert_eq!(swipes.len(), 1);
        // Endpoints scaled px/3, and duration in seconds (250ms -> 0.25s).
        assert_eq!(swipe_ends(swipes[0]), ((10.0, 20.0), (100.0, 200.0)));
        assert_eq!(swipes[0].duration, 0.25);
    }

    #[test]
    fn drag_without_duration_uses_default() {
        let inj = IdbInjector::new(3.0);
        let e = PointerEvent::Drag {
            from_x: 30,
            from_y: 60,
            to_x: 300,
            to_y: 600,
            button: MouseButton::Left,
            modifiers: vec![],
            duration_ms: 0,
        };
        let evts = inj.pointer_events(&e).unwrap();
        let swipes = swipes(&evts);
        assert_eq!(swipes.len(), 1);
        assert_eq!(swipes[0].duration, SWIPE_SECS);
    }

    #[test]
    fn scroll_emits_swipe_opposite_the_wheel() {
        let inj = IdbInjector::new(3.0);
        let e = PointerEvent::Scroll {
            x: 150,
            y: 150,
            dx: 0,
            dy: 1,
            modifiers: vec![],
        };
        let evts = inj.pointer_events(&e).unwrap();
        let swipes = swipes(&evts);
        assert_eq!(swipes.len(), 1);
        // Anchored at the point (50,50); ends one SCROLL_STEP_PX up the y-axis
        // (150 - 1*120 = 30 px -> 10 pt), i.e. opposite the +dy wheel direction.
        assert_eq!(swipe_ends(swipes[0]), ((50.0, 50.0), (50.0, 10.0)));
        assert_eq!(swipes[0].duration, SWIPE_SECS);
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use glass_core::KeyEvent;

    fn key_seq(evts: &[proto::HidEvent]) -> Vec<(u64, bool)> {
        // (keycode, is_down) for each press-key event, in order.
        evts.iter()
            .filter_map(|e| match &e.event {
                Some(proto::hid_event::Event::Press(p)) => {
                    let down = p.direction == proto::hid_event::HidDirection::Down as i32;
                    match p.action.as_ref().and_then(|a| a.action.as_ref()) {
                        Some(proto::hid_event::hid_press_action::Action::Key(k)) => {
                            Some((k.keycode, down))
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_lowercase_is_down_up() {
        let inj = IdbInjector::new(3.0);
        let evts = inj.key_events(&KeyEvent::Text("ab".into())).unwrap();
        assert_eq!(
            key_seq(&evts),
            vec![(0x04, true), (0x04, false), (0x05, true), (0x05, false)]
        );
    }

    #[test]
    fn text_uppercase_wraps_shift() {
        let inj = IdbInjector::new(3.0);
        let evts = inj.key_events(&KeyEvent::Text("A".into())).unwrap();
        // shift down, a down, a up, shift up
        assert_eq!(
            key_seq(&evts),
            vec![(0xE1, true), (0x04, true), (0x04, false), (0xE1, false)]
        );
    }

    #[test]
    fn chord_ctrl_a() {
        let inj = IdbInjector::new(3.0);
        let evts = inj.key_events(&KeyEvent::Chord("ctrl+a".into())).unwrap();
        assert_eq!(
            key_seq(&evts),
            vec![(0xE0, true), (0x04, true), (0x04, false), (0xE0, false)]
        );
    }

    #[test]
    fn chord_multi_modifier_releases_in_reverse() {
        let inj = IdbInjector::new(3.0);
        let evts = inj
            .key_events(&KeyEvent::Chord("ctrl+shift+a".into()))
            .unwrap();
        // Ctrl down, Shift down, a down, a up, then modifiers up in reverse:
        // Shift up, Ctrl up.
        assert_eq!(
            key_seq(&evts),
            vec![
                (0xE0, true),
                (0xE1, true),
                (0x04, true),
                (0x04, false),
                (0xE1, false),
                (0xE0, false),
            ]
        );
    }

    #[test]
    fn chord_unknown_modifier_errors() {
        let inj = IdbInjector::new(3.0);
        assert!(matches!(
            inj.key_events(&KeyEvent::Chord("hyper+x".into())),
            Err(glass_core::GlassError::InvalidKey(_))
        ));
    }

    #[test]
    fn chord_unknown_key_errors() {
        let inj = IdbInjector::new(3.0);
        assert!(matches!(
            inj.key_events(&KeyEvent::Chord("ctrl+nope".into())),
            Err(glass_core::GlassError::InvalidKey(_))
        ));
    }

    #[test]
    fn chord_empty_errors() {
        let inj = IdbInjector::new(3.0);
        assert!(matches!(
            inj.key_events(&KeyEvent::Chord("".into())),
            Err(glass_core::GlassError::InvalidKey(_))
        ));
    }

    #[test]
    fn text_unmapped_char_errors() {
        let inj = IdbInjector::new(3.0);
        assert!(matches!(
            inj.key_events(&KeyEvent::Text("€".into())),
            Err(glass_core::GlassError::Unsupported(_))
        ));
    }
}
