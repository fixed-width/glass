use super::*;
use crate::tools::start as start_tool;
use crate::tools::testutil::*;
use crate::tools::{OutContent, baseline_save};
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
        env: std::collections::BTreeMap::new(),
        window_hint: None,
        timeout_ms: None,
        a11y: None,
    };
    start_tool(&mut g, &a).unwrap();
    g
}

fn started_a11y(platform: FakePlatform) -> Glass {
    let mut g = glass_with_a11y(platform, fake_tree());
    let a = StartArgs {
        build: None,
        run: vec!["app".into()],
        backend: None,
        sandbox: None,
        cwd: None,
        env: std::collections::BTreeMap::new(),
        window_hint: None,
        timeout_ms: None,
        a11y: None,
    };
    start_tool(&mut g, &a).unwrap();
    crate::tools::a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    g
}

fn error_text(out: ToolOutput) -> String {
    match &out.0[0] {
        OutContent::Text(text) => text.clone(),
        OutContent::Image(_) => panic!("error envelope must be text"),
    }
}

fn click(x: i32, y: i32) -> Action {
    Action::Click(ClickArgs {
        x,
        y,
        button: None,
        count: None,
        modifiers: None,
    })
}

#[test]
fn success_retains_existing_fields_and_adds_every_step_result() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut g = started(FakePlatform::new(100, 100).with_event_log(log.clone()));
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![
                click(10, 20),
                Action::Type(TypeArgs {
                    text: "alice".into(),
                    return_: None,
                }),
                Action::Key(KeyArgs {
                    chord: "Tab".into(),
                }),
            ],
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        vec!["click(10,20)", "type(alice)", "key(Tab)"]
    );
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["executed"], json!(3));
    assert_eq!(result["steps"].as_array().unwrap().len(), 3);
    assert_eq!(result["steps"][0]["status"], "completed");
}

#[test]
fn type_return_snapshot_is_retained_in_content_blocks() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let frame = Frame::solid(100, 100, [0, 0, 0, 255]);
    let mut g = started_a11y(
        FakePlatform::new(100, 100)
            .with_event_log(log.clone())
            .with_frames(vec![frame.clone(), frame.clone(), frame]),
    );
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![Action::Type(TypeArgs {
                text: "hi".into(),
                return_: Some("snapshot".into()),
            })],
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["type(hi)"]);
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["executed"], json!(1));
    assert_eq!(result["steps"][0]["content_blocks"], json!([1]));
    assert_eq!(out.0.len(), 2, "snapshot outline is retained as a sibling");
    let OutContent::Text(snapshot) = &out.0[1] else {
        panic!("snapshot sibling must be text");
    };
    assert!(snapshot.contains("untrusted content"));
    assert!(snapshot.contains("Save"), "snapshot sibling: {snapshot}");
}

#[test]
fn type_action_with_return_none_is_allowed() {
    // "none" is the documented no-observe default — an explicit `"return":"none"`
    // is semantically identical to omitting the field and must not be rejected.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut g = started(FakePlatform::new(100, 100).with_event_log(log.clone()));
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![Action::Type(TypeArgs {
                text: "hi".into(),
                return_: Some("none".into()),
            })],
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["type(hi)"]);
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["executed"], json!(1));
}

#[test]
fn action_failure_is_structured_and_lists_unexecuted_steps() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut g = started(FakePlatform::new(100, 100).with_event_log(log.clone()));
    let err = error_text(
        do_actions(
            &mut g,
            &DoArgs {
                actions: vec![
                    click(10, 10),  // ok
                    click(100, 10), // out of bounds (valid 0..=99) -> fails
                    Action::Key(KeyArgs {
                        chord: "Return".into(),
                    }), // never runs
                ],
                then: None,
                timeout_ms: None,
                encoded_argument_bytes: 0,
            },
        )
        .unwrap_err(),
    );
    let error: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(error["error"]["code"], "step_failed");
    assert_eq!(error["error"]["step"], 1);
    assert_eq!(error["error"]["summary"], "action execution failed");
    assert_eq!(
        error["outcome"]["steps"][1]["error"]["summary"],
        "action execution failed"
    );
    assert!(!err.contains("coordinate (100,10)"));
    assert_eq!(error["outcome"]["executed"], 1);
    assert_eq!(error["outcome"]["steps"][2]["status"], "unexecuted");
    assert_eq!(
        *log.lock().unwrap(),
        vec!["click(10,10)"],
        "only the first action executed"
    );
}

#[test]
fn invalid_sequence_rejects_empty_actions_before_actuation() {
    let mut g = started(FakePlatform::new(10, 10));
    let err = error_text(
        do_actions(
            &mut g,
            &DoArgs {
                actions: vec![],
                then: None,
                timeout_ms: None,
                encoded_argument_bytes: 0,
            },
        )
        .unwrap_err(),
    );
    assert!(err.contains("at least one"), "got: {err}");
}

#[test]
fn semantic_actions_delegate_and_retain_standalone_results() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut g = started_a11y(FakePlatform::new(100, 100).with_event_log(log.clone()));
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![
                Action::ClickElement(ClickElementArgs {
                    id: 1,
                    return_: None,
                }),
                Action::SetValue(SetValueArgs {
                    id: 1,
                    text: "secret".into(),
                    return_: None,
                }),
                Action::WaitForElement(WaitForElementArgs {
                    name: Some("Save".into()),
                    description: None,
                    role: None,
                    condition: None,
                    value: None,
                    value_contains: None,
                    interval_ms: Some(0),
                    timeout_ms: Some(0),
                }),
                Action::ScrollToElement(ScrollToElementArgs {
                    name: Some("Save".into()),
                    description: None,
                    role: None,
                    value_contains: None,
                    direction: None,
                    x: None,
                    y: None,
                    step: None,
                    timeout_ms: Some(0),
                }),
            ],
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["click(20,20)"]);
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["steps"][0]["action"], "click_element");
    assert_eq!(result["steps"][1]["action"], "set_value");
    assert_eq!(result["steps"][2]["action"], "wait_for_element");
    assert_eq!(result["steps"][3]["action"], "scroll_to_element");
    assert_eq!(
        result["steps"][0]["result"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["id", "method", "native_fallback"]
    );
    assert_eq!(
        result["steps"][1]["result"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["id"]
    );
    assert_eq!(
        result["steps"][2]["result"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["elapsed_ms", "matched"]
    );
    assert_eq!(
        result["steps"][3]["result"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["elapsed_ms", "matched", "scrolled"]
    );
    assert!(!result.to_string().contains("secret"));
}

#[test]
fn click_element_stale_target_stops_with_structured_detail() {
    let mut g = started_a11y(FakePlatform::new(100, 100));
    let err = error_text(
        do_actions(
            &mut g,
            &DoArgs {
                actions: vec![
                    Action::ClickElement(ClickElementArgs {
                        id: 99,
                        return_: None,
                    }),
                    Action::Key(KeyArgs {
                        chord: "Tab".into(),
                    }),
                ],
                then: None,
                timeout_ms: None,
                encoded_argument_bytes: 0,
            },
        )
        .unwrap_err(),
    );
    let error: serde_json::Value = serde_json::from_str(&err).unwrap();
    let step = &error["outcome"]["steps"][0];
    assert_eq!(error["error"]["code"], "step_failed");
    assert_eq!(step["action"], "click_element");
    assert_eq!(step["attempted"], true);
    assert_eq!(step["side_effects_may_have_occurred"], true);
    assert!(step.get("result").is_none());
    assert_eq!(error["outcome"]["steps"][1]["status"], "unexecuted");
}

#[test]
fn wait_for_element_unmatched_fails_and_skips_the_rest() {
    let mut g = started_a11y(FakePlatform::new(100, 100));
    let err = error_text(
        do_actions(
            &mut g,
            &DoArgs {
                actions: vec![
                    Action::WaitForElement(WaitForElementArgs {
                        name: Some("missing".into()),
                        description: None,
                        role: None,
                        condition: None,
                        value: None,
                        value_contains: None,
                        interval_ms: Some(0),
                        timeout_ms: Some(0),
                    }),
                    Action::Key(KeyArgs {
                        chord: "Tab".into(),
                    }),
                ],
                then: None,
                timeout_ms: None,
                encoded_argument_bytes: 0,
            },
        )
        .unwrap_err(),
    );
    let error: serde_json::Value = serde_json::from_str(&err).unwrap();
    let step = &error["outcome"]["steps"][0];
    assert_eq!(error["error"]["code"], "predicate_not_matched");
    assert_eq!(step["result"]["matched"], false);
    assert!(step["result"].get("elapsed_ms").is_some());
    assert_eq!(step["attempted"], true);
    assert_eq!(step["side_effects_may_have_occurred"], false);
    assert_eq!(error["outcome"]["steps"][1]["status"], "unexecuted");
}

#[test]
fn scroll_to_element_unmatched_warns_that_side_effects_may_have_occurred() {
    let mut g = started_a11y(FakePlatform::new(100, 100));
    let err = error_text(
        do_actions(
            &mut g,
            &DoArgs {
                actions: vec![Action::ScrollToElement(ScrollToElementArgs {
                    name: Some("missing".into()),
                    description: None,
                    role: None,
                    value_contains: None,
                    direction: Some("down".into()),
                    x: None,
                    y: None,
                    step: None,
                    timeout_ms: Some(0),
                })],
                then: None,
                timeout_ms: None,
                encoded_argument_bytes: 0,
            },
        )
        .unwrap_err(),
    );
    let error: serde_json::Value = serde_json::from_str(&err).unwrap();
    let step = &error["outcome"]["steps"][0];
    assert_eq!(error["error"]["code"], "predicate_not_matched");
    assert_eq!(step["result"]["matched"], false);
    assert!(step["result"].get("scrolled").is_some());
    assert_eq!(step["side_effects_may_have_occurred"], true);
}

#[test]
fn semantic_return_snapshot_keeps_untrusted_outline_outside_the_envelope() {
    let mut g = started_a11y(FakePlatform::new(100, 100));
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![Action::ClickElement(ClickElementArgs {
                id: 1,
                return_: Some("snapshot".into()),
            })],
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["steps"][0]["content_blocks"], json!([1]));
    assert!(!result.to_string().contains("Save"));
    let OutContent::Text(outline) = &out.0[1] else {
        panic!("snapshot outline must be text");
    };
    assert!(outline.contains("untrusted content"));
    assert!(outline.contains("Save"));
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
                    ignore: None,
                }),
                diff: None,
                screenshot: None,
            }),
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(
        out.0.len(),
        1,
        "settle folded into the envelope, no separate/image block"
    );
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["then"]["settle"]["settled"], json!(true));
}

#[test]
fn then_settle_ignore_masks_a_blinking_pixel_so_it_settles() {
    // `settle_args()` must forward `SettleArgs.ignore` into `WaitStableParams.ignore`:
    // with no `#[serde(deny_unknown_fields)]` in this crate, a dropped field still parses
    // and just does nothing. Pixel (1,1) blinks across the three scripted frames while
    // the rest of the 2x2 stays constant, so only masking it settles within
    // `settle_frames`.
    //
    // Pinning the capture count to 3 rules out settling by outlasting the frames into
    // `FakePlatform`'s repeat-forever fallback.
    let log = Arc::new(Mutex::new(0usize));
    let mut f0 = Frame::solid(2, 2, [10, 10, 10, 255]);
    let mut f1 = f0.clone();
    let mut f2 = f0.clone();
    let idx = 3 * 4; // pixel (1,1): row 1 * width 2 + col 1 = 3, 4 bytes/pixel
    f0.pixels[idx] = 10;
    f1.pixels[idx] = 20;
    f2.pixels[idx] = 30;
    let mut g = started(
        FakePlatform::new(2, 2)
            .with_frames(vec![f0, f1, f2])
            .with_capture_log(log.clone()),
    );
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![click(0, 0)],
            then: Some(ThenArgs {
                settle: Some(SettleArgs {
                    interval_ms: Some(0),
                    settle_frames: Some(2),
                    tolerance: None,
                    timeout_ms: Some(1000),
                    stability_region: None,
                    ignore: Some(vec![RegionArgs {
                        x: 1,
                        y: 1,
                        width: 1,
                        height: 1,
                    }]),
                }),
                diff: None,
                screenshot: None,
            }),
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(
        result["then"]["settle"]["settled"],
        json!(true),
        "the blinking pixel is masked, so the stream is stable: {result}"
    );
    assert_eq!(
        result["then"]["settle"]["saw_motion"],
        json!(false),
        "masked motion must never set saw_motion: {result}"
    );
    assert_eq!(
        *log.lock().unwrap(),
        3,
        "must settle on the 3 supplied frames, not by outlasting them into FakePlatform's repeat"
    );
}

#[test]
fn then_screenshot_appends_image() {
    let mut g =
        started(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [1, 2, 3, 255])]));
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![click(1, 1)],
            then: Some(ThenArgs {
                settle: None,
                diff: None,
                screenshot: Some(ScreenshotArgs {
                    region: None,
                    window_id: None,
                }),
            }),
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["executed"], json!(1));
    assert_eq!(result["then"]["screenshot"]["width"], json!(4));
    assert!(
        matches!(out.0[1], OutContent::Image(_)),
        "screenshot image appended"
    );
    assert_eq!(
        out.0.len(),
        3,
        "envelope + screenshot image + IMAGE_NOTE (dims folded into result.then.screenshot)"
    );
    assert!(
        matches!(&out.0[2], OutContent::Text(t) if *t == crate::untrusted::IMAGE_NOTE),
        "IMAGE_NOTE last"
    );
}

#[test]
fn then_settle_timeout_still_succeeds() {
    // settle_frames=2 but timeout_ms=0 -> one tick, never settles -> settled:false,
    // yet do_actions returns Ok (a settle timeout is not a batch failure).
    let mut g =
        started(FakePlatform::new(2, 2).with_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]));
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
                    ignore: None,
                }),
                diff: None,
                screenshot: None,
            }),
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["then"]["settle"]["settled"], json!(false));
}

#[test]
fn then_diff_reports_change_text_only() {
    let base = Frame::solid(2, 2, [0, 0, 0, 255]);
    let mut changed = base.clone();
    changed.pixels[0] = 255;
    let mut g = started(FakePlatform::new(2, 2).with_frames(vec![base, changed]));
    baseline_save(&mut g, &BaselineSaveArgs { name: "m".into() }).unwrap();
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![click(0, 0)],
            then: Some(ThenArgs {
                settle: None,
                diff: Some(DiffArgs {
                    region: None,
                    name: "m".into(),
                    mode: None,
                    threshold: None,
                    tolerance: None,
                    include_image: Some(false),
                    ignore: None,
                }),
                screenshot: None,
            }),
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(
        out.0.len(),
        1,
        "no image -> the envelope alone, no nested envelope"
    );
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["then"]["diff"]["changed_pixels"], json!(1));
}

#[test]
fn then_diff_with_image_appends_image_sibling() {
    let base = Frame::solid(2, 2, [0, 0, 0, 255]);
    let mut changed = base.clone();
    changed.pixels[0] = 255;
    let mut g = started(FakePlatform::new(2, 2).with_frames(vec![base, changed]));
    baseline_save(&mut g, &BaselineSaveArgs { name: "m".into() }).unwrap();
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: vec![click(0, 0)],
            then: Some(ThenArgs {
                settle: None,
                diff: Some(DiffArgs {
                    region: None,
                    name: "m".into(),
                    mode: None,
                    threshold: None,
                    tolerance: None,
                    include_image: Some(true),
                    ignore: None,
                }),
                screenshot: None,
            }),
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    let result = assert_envelope(&out, "glass_do");
    assert_eq!(result["then"]["diff"]["changed_pixels"], json!(1));
    assert_eq!(
        out.0.len(),
        3,
        "envelope + diff image + IMAGE_NOTE (metrics folded into result.then.diff)"
    );
    assert!(
        matches!(out.0[1], OutContent::Image(_)),
        "diff's changed-region image rides alongside as a sibling"
    );
    assert!(
        matches!(&out.0[2], OutContent::Text(t) if *t == crate::untrusted::IMAGE_NOTE),
        "IMAGE_NOTE follows the image"
    );
}

#[test]
fn terminal_failure_keeps_completed_action_steps() {
    let mut g =
        started(FakePlatform::new(2, 2).with_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]));
    let err = error_text(
        do_actions(
            &mut g,
            &DoArgs {
                actions: vec![click(0, 0)],
                then: Some(ThenArgs {
                    settle: None,
                    diff: Some(DiffArgs {
                        region: None,
                        name: "absent".into(),
                        mode: None,
                        threshold: None,
                        tolerance: None,
                        include_image: None,
                        ignore: None,
                    }),
                    screenshot: None,
                }),
                timeout_ms: None,
                encoded_argument_bytes: 0,
            },
        )
        .unwrap_err(),
    );
    let error: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(error["error"]["code"], "terminal_observe_failed");
    assert_eq!(error["error"]["summary"], "terminal observation failed");
    assert!(!err.contains("baseline not found"));
    assert_eq!(error["outcome"]["executed"], 1);
}

#[test]
fn split_sub_requires_ok_and_tool_and_keeps_siblings() {
    // A well-formed sub-tool output (screenshot's shape): [Image, envelope, IMAGE_NOTE].
    // The envelope carries a `result` key alongside `ok`/`tool`; a bare JSON object
    // with only a `result` key (no `ok`/`tool`) must NOT match the tightened
    // predicate — it's included here as a leading sibling to prove that.
    let out = ToolOutput(vec![
        OutContent::Text(json!({ "result": "not the real envelope" }).to_string()),
        OutContent::Image(vec![1, 2, 3]),
        OutContent::Text(
            json!({ "ok": true, "tool": "glass_screenshot", "result": { "width": 4 } }).to_string(),
        ),
        OutContent::Text(crate::untrusted::IMAGE_NOTE.to_string()),
    ]);
    let (result, siblings) = split_sub(out);
    assert_eq!(
        result,
        json!({ "width": 4 }),
        "real envelope's result extracted"
    );
    assert_eq!(
        siblings.len(),
        3,
        "the fake-envelope text, image, and IMAGE_NOTE all ride as siblings"
    );
    assert!(
        matches!(&siblings[0], OutContent::Text(t) if t.contains("not the real envelope")),
        "JSON with `result` but no ok/tool is not misclassified as the envelope"
    );
    assert!(
        matches!(siblings[1], OutContent::Image(_)),
        "image sibling preserved"
    );
    assert!(
        matches!(&siblings[2], OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE),
        "IMAGE_NOTE sibling preserved"
    );
}

fn limit_actions(n: usize) -> Vec<Action> {
    (0..n).map(|_| click(0, 0)).collect()
}

fn error_code(out: ToolOutput) -> String {
    let text = error_text(out);
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"]["code"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn invalid_sequence_rejects_sixty_five_actions_before_actuation() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut g = started(FakePlatform::new(10, 10).with_event_log(log.clone()));
    assert_eq!(
        error_code(
            do_actions(
                &mut g,
                &DoArgs {
                    actions: limit_actions(MAX_ACTIONS + 1),
                    then: None,
                    timeout_ms: None,
                    encoded_argument_bytes: 0,
                }
            )
            .unwrap_err()
        ),
        "invalid_sequence"
    );
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn invalid_sequence_accepts_exact_action_limit() {
    let mut g = started(FakePlatform::new(10, 10));
    let out = do_actions(
        &mut g,
        &DoArgs {
            actions: limit_actions(MAX_ACTIONS),
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(assert_envelope(&out, "glass_do")["executed"], MAX_ACTIONS);
}

#[test]
fn invalid_sequence_rejects_oversized_compact_arguments() {
    let raw = format!(
        r#"{{"actions":[{{"action":"type","text":"{}"}}]}}"#,
        "x".repeat(MAX_ARGUMENT_BYTES)
    );
    let a: DoArgs = serde_json::from_str(&raw).unwrap();
    let mut g = started(FakePlatform::new(10, 10));
    assert_eq!(
        error_code(do_actions(&mut g, &a).unwrap_err()),
        "invalid_sequence"
    );
}

#[test]
fn invalid_sequence_accepts_exact_byte_limit() {
    let overhead = r#"{"actions":[{"action":"type","text":""}]}"#.len();
    let raw = format!(
        r#"{{"actions":[{{"action":"type","text":"{}"}}]}}"#,
        "x".repeat(MAX_ARGUMENT_BYTES - overhead)
    );
    let a: DoArgs = serde_json::from_str(&raw).unwrap();
    assert_eq!(a.encoded_argument_bytes, MAX_ARGUMENT_BYTES);
    let mut g = started(FakePlatform::new(10, 10));
    assert!(do_actions(&mut g, &a).is_ok());
}

#[test]
fn invalid_sequence_rejects_zero_and_over_max_timeout() {
    let mut g = started(FakePlatform::new(10, 10));
    for timeout_ms in [Some(0), Some(MAX_TIMEOUT_MS + 1)] {
        assert_eq!(
            error_code(
                do_actions(
                    &mut g,
                    &DoArgs {
                        actions: vec![click(0, 0)],
                        then: None,
                        timeout_ms,
                        encoded_argument_bytes: 0,
                    }
                )
                .unwrap_err()
            ),
            "invalid_sequence"
        );
    }
}
