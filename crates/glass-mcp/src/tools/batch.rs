//! `glass_do`: run an ordered input sequence server-side, then optionally observe.

use glass_core::Glass;
use serde_json::json;

use crate::params::*;
use crate::tools::{
    click, diff, drag, key, mouse_move, screenshot, scroll, type_text, wait_stable, OutContent,
    ToolOutput, ToolResult,
};

/// Build a text-only `WaitStableArgs` from a `SettleArgs` (no image, no crop).
fn settle_args(s: &SettleArgs) -> WaitStableArgs {
    WaitStableArgs {
        interval_ms: s.interval_ms,
        settle_frames: s.settle_frames,
        tolerance: s.tolerance,
        timeout_ms: s.timeout_ms,
        region: None,
        stability_region: s.stability_region.clone(),
        include_image: Some(false),
    }
}

/// Run an ordered action sequence, then the optional terminal observe.
/// Fail-fast: the first failing action aborts with its index/kind/message and
/// the count that ran. A `then` failure is reported distinctly (the actions
/// already executed).
pub fn do_actions(glass: &mut Glass, a: &DoArgs) -> ToolResult {
    if a.actions.is_empty() {
        return Err("`actions` must contain at least one action".into());
    }
    let n = a.actions.len();
    for (i, action) in a.actions.iter().enumerate() {
        let (kind, result): (&str, ToolResult) = match action {
            Action::Click(args) => ("click", click(glass, args)),
            Action::Move(args) => ("move", mouse_move(glass, args)),
            Action::Drag(args) => ("drag", drag(glass, args)),
            Action::Scroll(args) => ("scroll", scroll(glass, args)),
            Action::Type(args) => ("type", type_text(glass, args)),
            Action::Key(args) => ("key", key(glass, args)),
            // A settle's text-only output is discarded mid-sequence; only its
            // Err (bad region / capture failure) aborts. A non-settle (timeout)
            // is Ok and proceeds.
            Action::Settle(args) => ("settle", wait_stable(glass, &settle_args(args))),
        };
        if let Err(msg) = result {
            return Err(format!(
                "action[{i}] ({kind}) failed: {msg} — {i} of {n} actions executed before the failure"
            ));
        }
    }

    let mut out = vec![OutContent::Text(json!({ "executed": n }).to_string())];
    if let Some(then) = &a.then {
        let mut items = run_then(glass, then)
            .map_err(|msg| format!("all {n} actions executed; terminal observe failed: {msg}"))?;
        out.append(&mut items);
    }
    Ok(ToolOutput(out))
}

/// Run the terminal observe in fixed order: settle → diff → screenshot.
fn run_then(glass: &mut Glass, then: &ThenArgs) -> Result<Vec<OutContent>, String> {
    let mut items = Vec::new();
    if let Some(s) = &then.settle {
        items.extend(wait_stable(glass, &settle_args(s))?.0);
    }
    if let Some(d) = &then.diff {
        items.extend(diff(glass, d)?.0);
    }
    if let Some(sc) = &then.screenshot {
        items.extend(screenshot(glass, sc)?.0);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::start as start_tool;
    use crate::tools::testutil::*;
    use crate::tools::OutContent;
    use glass_core::Frame;
    use std::sync::{Arc, Mutex};

    fn started(platform: FakePlatform) -> Glass {
        let mut g = glass_with(platform);
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

    fn click(x: i32, y: i32) -> Action {
        Action::Click(ClickArgs { x, y, button: None, count: None, modifiers: None })
    }

    #[test]
    fn runs_actions_in_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = started(FakePlatform::new(100, 100).with_event_log(log.clone()));
        let out = do_actions(
            &mut g,
            &DoArgs {
                actions: vec![
                    click(10, 20),
                    Action::Type(TypeArgs { text: "alice".into() }),
                    Action::Key(KeyArgs { chord: "Tab".into() }),
                ],
                then: None,
            },
        )
        .unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["click(10,20)", "type(alice)", "key(Tab)"]);
        match &out.0[0] {
            OutContent::Text(t) => assert!(t.contains("\"executed\":3"), "got: {t}"),
            _ => panic!("expected executed text"),
        }
    }

    #[test]
    fn fail_fast_reports_index_and_stops() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = started(FakePlatform::new(100, 100).with_event_log(log.clone()));
        let err = do_actions(
            &mut g,
            &DoArgs {
                actions: vec![
                    click(10, 10),  // ok
                    click(100, 10), // out of bounds (valid 0..=99) -> fails
                    Action::Key(KeyArgs { chord: "Return".into() }), // never runs
                ],
                then: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("action[1]"), "got: {err}");
        assert!(err.contains("click"), "got: {err}");
        assert!(err.contains("1 of 3"), "got: {err}");
        assert_eq!(*log.lock().unwrap(), vec!["click(10,10)"], "only the first action executed");
    }

    #[test]
    fn empty_actions_rejected() {
        let mut g = started(FakePlatform::new(10, 10));
        let err = do_actions(&mut g, &DoArgs { actions: vec![], then: None }).unwrap_err();
        assert!(err.contains("at least one"), "got: {err}");
    }

    #[test]
    fn then_settle_is_text_only() {
        let f = Frame::solid(2, 2, [5, 5, 5, 255]);
        let mut g = started(FakePlatform::new(2, 2).with_frames(vec![f.clone(), f]));
        let out = do_actions(
            &mut g,
            &DoArgs {
                actions: vec![click(0, 0)],
                then: Some(ThenArgs {
                    settle: Some(SettleArgs {
                        interval_ms: Some(0),
                        settle_frames: Some(2),
                        tolerance: None,
                        timeout_ms: Some(200),
                        stability_region: None,
                    }),
                    diff: None,
                    screenshot: None,
                }),
            },
        )
        .unwrap();
        assert_eq!(out.0.len(), 2, "executed text + settle text, no image");
        match &out.0[1] {
            OutContent::Text(t) => assert!(t.contains("\"settled\":true"), "got: {t}"),
            _ => panic!("expected settle text, not an image"),
        }
    }

    #[test]
    fn then_screenshot_appends_image() {
        let mut g = started(
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [1, 2, 3, 255])]),
        );
        let out = do_actions(
            &mut g,
            &DoArgs {
                actions: vec![click(1, 1)],
                then: Some(ThenArgs {
                    settle: None,
                    diff: None,
                    screenshot: Some(ScreenshotArgs { region: None }),
                }),
            },
        )
        .unwrap();
        assert!(matches!(&out.0[0], OutContent::Text(t) if t.contains("\"executed\":1")));
        assert!(matches!(out.0[1], OutContent::Image(_)), "screenshot image appended");
        assert_eq!(out.0.len(), 4, "executed text + screenshot image + dims text + IMAGE_NOTE");
        assert!(matches!(&out.0[2], OutContent::Text(t) if t.contains("\"width\":4")), "dims text after image");
        assert!(matches!(&out.0[3], OutContent::Text(t) if *t == crate::untrusted::IMAGE_NOTE), "IMAGE_NOTE last");
    }

    #[test]
    fn then_settle_timeout_still_succeeds() {
        // settle_frames=2 but timeout_ms=0 -> one tick, never settles -> settled:false,
        // yet do_actions returns Ok (a settle timeout is not a batch failure).
        let mut g = started(FakePlatform::new(2, 2).with_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]));
        let out = do_actions(
            &mut g,
            &DoArgs {
                actions: vec![click(0, 0)],
                then: Some(ThenArgs {
                    settle: Some(SettleArgs {
                        interval_ms: Some(0),
                        settle_frames: Some(2),
                        tolerance: None,
                        timeout_ms: Some(0),
                        stability_region: None,
                    }),
                    diff: None,
                    screenshot: None,
                }),
            },
        )
        .unwrap();
        match &out.0[1] {
            OutContent::Text(t) => assert!(t.contains("\"settled\":false"), "got: {t}"),
            _ => panic!("expected settle text"),
        }
    }

    #[test]
    fn terminal_observe_failure_is_distinct() {
        let mut g = started(
            FakePlatform::new(2, 2).with_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]),
        );
        let err = do_actions(
            &mut g,
            &DoArgs {
                actions: vec![click(0, 0)],
                then: Some(ThenArgs {
                    settle: None,
                    diff: Some(DiffArgs {
                        name: "absent".into(),
                        mode: None,
                        threshold: None,
                        tolerance: None,
                        include_image: None,
                    }),
                    screenshot: None,
                }),
            },
        )
        .unwrap_err();
        assert!(err.contains("all 1 actions executed"), "got: {err}");
        assert!(err.contains("terminal observe failed"), "got: {err}");
        assert!(err.contains("baseline"), "got: {err}");
    }
}
