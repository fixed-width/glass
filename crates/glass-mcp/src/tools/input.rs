//! Pointer and keyboard tools.

use glass_core::{Glass, KeyEvent, Modifier, PointerEvent};

use crate::params::*;
use crate::tools::{parse_button, ToolOutput, ToolResult};

pub(crate) fn parse_modifiers(mods: Option<&[String]>) -> Result<Vec<Modifier>, String> {
    let mut out = Vec::new();
    for m in mods.unwrap_or(&[]) {
        out.push(
            Modifier::from_name(m)
                .ok_or_else(|| format!("unknown modifier '{m}' (use ctrl/shift/alt/super)"))?,
        );
    }
    Ok(out)
}

pub fn click(glass: &mut Glass, a: &ClickArgs) -> ToolResult {
    let button = parse_button(a.button.as_deref())?;
    let modifiers = parse_modifiers(a.modifiers.as_deref())?;
    glass
        .pointer(&PointerEvent::Click {
            x: a.x,
            y: a.y,
            button,
            count: a.count.unwrap_or(1),
            modifiers,
        })
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("ok"))
}

pub fn mouse_move(glass: &mut Glass, a: &MoveArgs) -> ToolResult {
    glass
        .pointer(&PointerEvent::Move { x: a.x, y: a.y })
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("ok"))
}

pub fn drag(glass: &mut Glass, a: &DragArgs) -> ToolResult {
    let button = parse_button(a.button.as_deref())?;
    let modifiers = parse_modifiers(a.modifiers.as_deref())?;
    glass
        .pointer(&PointerEvent::Drag {
            from_x: a.x1,
            from_y: a.y1,
            to_x: a.x2,
            to_y: a.y2,
            button,
            modifiers,
            duration_ms: a.duration_ms.unwrap_or(200).min(10_000),
        })
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("ok"))
}

pub fn gesture(glass: &mut Glass, a: &GestureArgs) -> ToolResult {
    let n = a.pointers.len();
    if n < 2 {
        return Err("glass_gesture needs 2+ pointers; use glass_drag for a single pointer".into());
    }
    if n > glass_core::MAX_GESTURE_POINTERS {
        return Err(format!(
            "too many pointers ({n}); max is {}",
            glass_core::MAX_GESTURE_POINTERS
        ));
    }
    let pointers = a
        .pointers
        .iter()
        .map(|p| glass_core::Segment {
            from_x: p.from.x,
            from_y: p.from.y,
            to_x: p.to.x,
            to_y: p.to.y,
        })
        .collect();
    glass
        .pointer(&PointerEvent::Gesture {
            pointers,
            duration_ms: a.duration_ms.unwrap_or(250).min(10_000),
        })
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("ok"))
}

pub fn scroll(glass: &mut Glass, a: &ScrollArgs) -> ToolResult {
    let modifiers = parse_modifiers(a.modifiers.as_deref())?;
    glass
        .pointer(&PointerEvent::Scroll {
            x: a.x,
            y: a.y,
            dx: a.dx.unwrap_or(0),
            dy: a.dy.unwrap_or(0),
            modifiers,
        })
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("ok"))
}

pub fn type_text(glass: &mut Glass, a: &TypeArgs) -> ToolResult {
    glass
        .key(&KeyEvent::Text(a.text.clone()))
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("ok"))
}

pub fn key(glass: &mut Glass, a: &KeyArgs) -> ToolResult {
    glass
        .key(&KeyEvent::Chord(a.chord.clone()))
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("ok"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::*;
    use crate::tools::{start as start_tool, OutContent};

    fn started() -> Glass {
        let mut g = glass_with(FakePlatform::new(100, 100));
        let a = StartArgs {
            build: None,
            run: vec!["app".into()],
            backend: None,
            sandbox: None,
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: None,
            a11y: None,
        };
        start_tool(&mut g, &a).unwrap();
        g
    }

    fn text(out: &ToolOutput) -> &str {
        match &out.0[0] {
            OutContent::Text(t) => t,
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn click_in_bounds_ok() {
        let mut g = started();
        let a = ClickArgs {
            x: 10,
            y: 20,
            button: None,
            count: None,
            modifiers: None,
        };
        assert_eq!(text(&click(&mut g, &a).unwrap()), "ok");
    }

    #[test]
    fn click_out_of_bounds_errors() {
        let mut g = started();
        let a = ClickArgs {
            x: 100,
            y: 20,
            button: None,
            count: None,
            modifiers: None,
        }; // valid 0..=99
        assert!(click(&mut g, &a).unwrap_err().contains("out of bounds"));
    }

    #[test]
    fn bad_button_errors() {
        let mut g = started();
        let a = ClickArgs {
            x: 1,
            y: 1,
            button: Some("nope".into()),
            count: None,
            modifiers: None,
        };
        assert!(click(&mut g, &a).unwrap_err().contains("unknown button"));
    }

    #[test]
    fn type_and_key_ok() {
        let mut g = started();
        assert_eq!(
            text(&type_text(&mut g, &TypeArgs { text: "hi".into() }).unwrap()),
            "ok"
        );
        assert_eq!(
            text(
                &key(
                    &mut g,
                    &KeyArgs {
                        chord: "ctrl+s".into()
                    }
                )
                .unwrap()
            ),
            "ok"
        );
    }

    #[test]
    fn drag_and_scroll_ok() {
        let mut g = started();
        let d = DragArgs {
            x1: 1,
            y1: 2,
            x2: 3,
            y2: 4,
            button: None,
            modifiers: None,
            duration_ms: None,
        };
        assert_eq!(text(&drag(&mut g, &d).unwrap()), "ok");
        let s = ScrollArgs {
            x: 5,
            y: 6,
            dx: None,
            dy: Some(2),
            modifiers: None,
        };
        assert_eq!(text(&scroll(&mut g, &s).unwrap()), "ok");
    }

    #[test]
    fn gesture_two_pointers_ok() {
        let mut g = started();
        let a = GestureArgs {
            pointers: vec![
                PointerArgs {
                    from: PointArg { x: 30, y: 40 },
                    to: PointArg { x: 10, y: 40 },
                },
                PointerArgs {
                    from: PointArg { x: 50, y: 40 },
                    to: PointArg { x: 70, y: 40 },
                },
            ],
            duration_ms: Some(120),
        };
        assert_eq!(text(&gesture(&mut g, &a).unwrap()), "ok");
    }

    #[test]
    fn gesture_one_pointer_errors() {
        let mut g = started();
        let a = GestureArgs {
            pointers: vec![PointerArgs {
                from: PointArg { x: 1, y: 1 },
                to: PointArg { x: 2, y: 2 },
            }],
            duration_ms: None,
        };
        assert!(gesture(&mut g, &a).is_err());
    }

    #[test]
    fn click_parses_and_rejects_modifiers() {
        let mut g = started();
        let ok = ClickArgs {
            x: 1,
            y: 1,
            button: None,
            count: None,
            modifiers: Some(vec!["ctrl".into()]),
        };
        assert_eq!(text(&click(&mut g, &ok).unwrap()), "ok");
        let bad = ClickArgs {
            x: 1,
            y: 1,
            button: None,
            count: None,
            modifiers: Some(vec!["hyper".into()]),
        };
        assert!(click(&mut g, &bad)
            .unwrap_err()
            .contains("unknown modifier"));
    }
}
