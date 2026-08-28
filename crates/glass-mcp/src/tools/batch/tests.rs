use super::*;
use crate::tools::start as start_tool;
use crate::tools::testutil::*;
use crate::tools::{OutContent, baseline_save};
use glass_core::{
    Accessibility, AppSpec, AxContext, AxTree, Backend, BaselineStore, Deadline, Frame, GlassError,
    KeyEvent, Platform, PlatformFactory, PointerEvent, Region, Result as GlassResult, Stream,
    WindowGeometry, WindowId, WindowInfo, WindowOp,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum DeadlineBehavior {
    Normal,
    CompleteLate,
    FailLate,
    CaptureCompletesLate,
}

struct DeadlinePlatform {
    inner: FakePlatform,
    deadlines: Arc<Mutex<Vec<Deadline>>>,
    behavior: DeadlineBehavior,
}

type DeadlineFixture = (Glass, Arc<Mutex<Vec<Deadline>>>, Arc<Mutex<Vec<String>>>);
type A11yDeadlineFixture = (Glass, Arc<Mutex<Vec<Deadline>>>, Arc<Mutex<Vec<Deadline>>>);

struct DeadlineAccessibility {
    tree: AxTree,
    deadlines: Arc<Mutex<Vec<Deadline>>>,
}

impl Accessibility for DeadlineAccessibility {
    fn snapshot(&mut self, context: &AxContext) -> GlassResult<AxTree> {
        self.deadlines.lock().unwrap().push(context.deadline);
        Ok(self.tree.clone())
    }
}

impl Platform for DeadlinePlatform {
    fn start_app(&mut self, spec: &AppSpec) -> GlassResult<WindowGeometry> {
        self.inner.start_app(spec)
    }
    fn stop_app_by(&mut self, deadline: Deadline) -> GlassResult<()> {
        self.inner.stop_app_by(deadline)
    }
    fn capture_frame(&mut self, region: Option<&Region>) -> GlassResult<Frame> {
        self.inner.capture_frame(region)
    }
    fn capture_frame_by(
        &mut self,
        region: Option<&Region>,
        deadline: Deadline,
    ) -> GlassResult<Frame> {
        self.deadlines.lock().unwrap().push(deadline);
        if matches!(self.behavior, DeadlineBehavior::CaptureCompletesLate) {
            std::thread::sleep(Duration::from_millis(160));
            return self.inner.capture_frame(region);
        }
        self.inner.capture_frame(region)
    }
    fn send_pointer(&mut self, event: &PointerEvent) -> GlassResult<()> {
        self.inner.send_pointer(event)
    }
    fn send_pointer_by(&mut self, event: &PointerEvent, deadline: Deadline) -> GlassResult<()> {
        self.deadlines.lock().unwrap().push(deadline);
        match self.behavior {
            DeadlineBehavior::Normal => self.inner.send_pointer(event),
            DeadlineBehavior::CompleteLate => {
                std::thread::sleep(Duration::from_millis(3));
                self.inner.send_pointer(event)
            }
            DeadlineBehavior::FailLate => {
                self.inner.send_pointer(event)?;
                std::thread::sleep(Duration::from_millis(3));
                Err(GlassError::caller_deadline_elapsed("controlled pointer"))
            }
            DeadlineBehavior::CaptureCompletesLate => self.inner.send_pointer(event),
        }
    }
    fn send_key(&mut self, event: &KeyEvent) -> GlassResult<()> {
        self.inner.send_key(event)
    }
    fn send_key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> GlassResult<()> {
        self.deadlines.lock().unwrap().push(deadline);
        self.inner.send_key(event)
    }
    fn window(&mut self, op: &WindowOp) -> GlassResult<WindowGeometry> {
        self.inner.window(op)
    }
    fn list_windows(&mut self) -> GlassResult<Vec<WindowInfo>> {
        self.inner.list_windows()
    }
    fn select_window(&mut self, id: WindowId) -> GlassResult<WindowGeometry> {
        self.inner.select_window(id)
    }
    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        self.inner.drain_logs()
    }
}

fn deadline_glass(behavior: DeadlineBehavior, frames: Vec<Frame>) -> DeadlineFixture {
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let platform = DeadlinePlatform {
        inner: FakePlatform::new(100, 100)
            .with_frames(frames)
            .with_event_log(events.clone()),
        deadlines: deadlines.clone(),
        behavior,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    let mut held = Some(Box::new(platform) as Box<dyn Platform + Send>);
    let factory: PlatformFactory = Box::new(move |_| {
        Ok(Backend::display_only(held.take().ok_or_else(|| {
            GlassError::Backend("factory called twice".into())
        })?))
    });
    let mut glass = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
    start_tool(
        &mut glass,
        &StartArgs {
            build: None,
            run: vec!["app".into()],
            backend: None,
            sandbox: None,
            cwd: None,
            env: Default::default(),
            window_hint: None,
            timeout_ms: None,
            a11y: None,
        },
    )
    .unwrap();
    deadlines.lock().unwrap().clear();
    (glass, deadlines, events)
}

fn deadline_a11y_glass(frames: Vec<Frame>) -> A11yDeadlineFixture {
    let platform_deadlines = Arc::new(Mutex::new(Vec::new()));
    let accessibility_deadlines = Arc::new(Mutex::new(Vec::new()));
    let platform = DeadlinePlatform {
        inner: FakePlatform::new(100, 100).with_frames(frames),
        deadlines: platform_deadlines.clone(),
        behavior: DeadlineBehavior::Normal,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    let mut held = Some(Backend {
        platform: Box::new(platform),
        accessibility: Some(Box::new(DeadlineAccessibility {
            tree: fake_tree(),
            deadlines: accessibility_deadlines.clone(),
        })),
    });
    let factory: PlatformFactory = Box::new(move |_| {
        held.take()
            .ok_or_else(|| GlassError::Backend("factory called twice".into()))
    });
    let mut glass = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
    start_tool(
        &mut glass,
        &StartArgs {
            build: None,
            run: vec!["app".into()],
            backend: None,
            sandbox: None,
            cwd: None,
            env: Default::default(),
            window_hint: None,
            timeout_ms: None,
            a11y: None,
        },
    )
    .unwrap();
    crate::tools::a11y_snapshot(&mut glass, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    platform_deadlines.lock().unwrap().clear();
    accessibility_deadlines.lock().unwrap().clear();
    (glass, platform_deadlines, accessibility_deadlines)
}

fn do_args(actions: Vec<Action>, timeout_ms: u64) -> DoArgs {
    DoArgs {
        actions,
        then: None,
        timeout_ms: Some(timeout_ms),
        encoded_argument_bytes: 0,
    }
}

#[test]
fn standalone_handlers_use_unbounded_context_and_keep_wire_shape() {
    let (mut g, deadlines, _) = deadline_glass(
        DeadlineBehavior::CaptureCompletesLate,
        vec![Frame::solid(100, 100, [0, 0, 0, 255])],
    );
    let args = ClickArgs {
        x: 1,
        y: 2,
        button: None,
        count: None,
        modifiers: None,
    };
    let standalone = crate::tools::click(&mut g, &args).unwrap();
    let contextual = crate::tools::click_with(&mut g, &args, crate::tools::ToolContext::UNBOUNDED)
        .unwrap()
        .output;
    assert_eq!(format!("{standalone:?}"), format!("{contextual:?}"));
    assert_eq!(
        *deadlines.lock().unwrap(),
        vec![Deadline::UNBOUNDED, Deadline::UNBOUNDED]
    );
    let screenshot_args = ScreenshotArgs {
        region: None,
        window_id: None,
    };
    let standalone_image = crate::tools::screenshot(&mut g, &screenshot_args).unwrap();
    let contextual_image = crate::tools::screenshot_with(
        &mut g,
        &screenshot_args,
        crate::tools::ToolContext::UNBOUNDED,
    )
    .unwrap()
    .output;
    assert_eq!(
        format!("{standalone_image:?}"),
        format!("{contextual_image:?}")
    );
    assert!(matches!(standalone_image.0[0], OutContent::Image(_)));
    let bad = ClickArgs { x: 100, ..args };
    assert_eq!(
        crate::tools::click(&mut g, &bad).unwrap_err(),
        crate::tools::click_with(&mut g, &bad, crate::tools::ToolContext::UNBOUNDED)
            .unwrap_err()
            .message
    );
}

#[test]
fn sequence_deadline_construction_is_checked() {
    let started = Instant::now();
    assert_eq!(
        checked_sequence_deadline(started, Duration::from_secs(1)),
        Some(Deadline::at(started + Duration::from_secs(1)))
    );
    assert!(checked_sequence_deadline(started, Duration::MAX).is_none());
}

#[test]
fn standalone_return_handlers_keep_wire_shape_and_unbounded_context() {
    let frame = Frame::solid(100, 100, [7, 8, 9, 255]);
    let settle_args = TypeArgs {
        text: "private".into(),
        return_: Some("settle".into()),
    };
    let (mut standalone, standalone_deadlines, _) = deadline_glass(
        DeadlineBehavior::Normal,
        vec![frame.clone(), frame.clone(), frame.clone()],
    );
    let (mut contextual, contextual_deadlines, _) = deadline_glass(
        DeadlineBehavior::Normal,
        vec![frame.clone(), frame.clone(), frame.clone()],
    );
    let standalone_out = crate::tools::type_text(&mut standalone, &settle_args).unwrap();
    let contextual_out = crate::tools::type_text_with(
        &mut contextual,
        &settle_args,
        crate::tools::ToolContext::UNBOUNDED,
    )
    .unwrap()
    .output;
    let OutContent::Text(standalone_envelope) = &standalone_out.0[0] else {
        panic!("standalone type settle envelope must lead")
    };
    let OutContent::Text(contextual_envelope) = &contextual_out.0[0] else {
        panic!("contextual type settle envelope must lead")
    };
    let mut standalone_value: serde_json::Value =
        serde_json::from_str(standalone_envelope).unwrap();
    let mut contextual_value: serde_json::Value =
        serde_json::from_str(contextual_envelope).unwrap();
    standalone_value["result"]["observed"]["observed_ms"] = json!(0);
    contextual_value["result"]["observed"]["observed_ms"] = json!(0);
    assert_eq!(standalone_value, contextual_value);
    assert_eq!(standalone_deadlines.lock().unwrap()[0], Deadline::UNBOUNDED);
    assert_eq!(contextual_deadlines.lock().unwrap()[0], Deadline::UNBOUNDED);

    let snapshot_args = TypeArgs {
        text: "private".into(),
        return_: Some("snapshot".into()),
    };
    let (mut standalone, standalone_platform, standalone_ax) =
        deadline_a11y_glass(vec![frame.clone(), frame.clone(), frame.clone()]);
    let (mut contextual, contextual_platform, contextual_ax) =
        deadline_a11y_glass(vec![frame.clone(), frame.clone(), frame]);
    let standalone_out = crate::tools::type_text(&mut standalone, &snapshot_args).unwrap();
    let contextual_out = crate::tools::type_text_with(
        &mut contextual,
        &snapshot_args,
        crate::tools::ToolContext::UNBOUNDED,
    )
    .unwrap()
    .output;
    let OutContent::Text(standalone_envelope) = &standalone_out.0[0] else {
        panic!("standalone type snapshot envelope must lead")
    };
    let OutContent::Text(contextual_envelope) = &contextual_out.0[0] else {
        panic!("contextual type snapshot envelope must lead")
    };
    assert_eq!(standalone_envelope, contextual_envelope);
    assert_eq!(standalone_out.0.len(), contextual_out.0.len());
    assert!(format!("{:?}", standalone_out.0[1]).contains("Save"));
    assert!(format!("{:?}", contextual_out.0[1]).contains("Save"));
    for log in [standalone_platform, contextual_platform] {
        let deadlines = log.lock().unwrap();
        assert!(!deadlines.is_empty());
        assert_eq!(deadlines[0], Deadline::UNBOUNDED);
    }
    for log in [standalone_ax, contextual_ax] {
        let deadlines = log.lock().unwrap();
        assert!(!deadlines.is_empty());
        assert!(deadlines.iter().all(|d| *d == Deadline::UNBOUNDED));
    }

    let invalid = TypeArgs {
        text: "private".into(),
        return_: Some("later".into()),
    };
    let mut standalone = started(FakePlatform::new(100, 100));
    let mut contextual = started(FakePlatform::new(100, 100));
    assert_eq!(
        crate::tools::type_text(&mut standalone, &invalid).unwrap_err(),
        crate::tools::type_text_with(
            &mut contextual,
            &invalid,
            crate::tools::ToolContext::UNBOUNDED,
        )
        .unwrap_err()
        .message
    );
}

#[test]
fn sequence_uses_one_absolute_deadline_for_every_action() {
    let frame = Frame::solid(100, 100, [0, 0, 0, 255]);
    let (mut g, deadlines, _) =
        deadline_glass(DeadlineBehavior::Normal, vec![frame.clone(), frame]);
    do_actions(
        &mut g,
        &do_args(
            vec![
                click(1, 1),
                Action::Key(KeyArgs {
                    chord: "Tab".into(),
                }),
                Action::Settle(SettleArgs {
                    interval_ms: Some(0),
                    settle_frames: Some(1),
                    tolerance: None,
                    timeout_ms: Some(2000),
                    stability_region: None,
                    ignore: None,
                }),
            ],
            1000,
        ),
    )
    .unwrap();
    let seen = deadlines.lock().unwrap();
    assert!(
        seen.len() >= 4,
        "pointer, key, and settle captures must be bounded: {seen:?}"
    );
    assert!(seen.iter().all(|deadline| *deadline == seen[0]));
    assert_ne!(seen[0], Deadline::UNBOUNDED);
}

#[test]
fn deadline_spent_before_step_marks_it_failed_unattempted() {
    let (mut g, _, events) = deadline_glass(DeadlineBehavior::CompleteLate, vec![]);
    let err = do_actions(
        &mut g,
        &do_args(
            vec![
                click(1, 1),
                Action::Key(KeyArgs {
                    chord: "Return".into(),
                }),
                click(2, 2),
            ],
            1,
        ),
    )
    .unwrap_err();
    let error = error_text(err);
    let envelope: serde_json::Value = serde_json::from_str(&error).unwrap();
    assert_eq!(envelope["error"]["code"], "sequence_deadline_exceeded");
    assert_eq!(envelope["outcome"]["steps"][1]["attempted"], false);
    assert_eq!(
        envelope["outcome"]["steps"][1]["side_effects_may_have_occurred"],
        false
    );
    assert_eq!(envelope["outcome"]["steps"][2]["status"], "unexecuted");
    assert_eq!(*events.lock().unwrap(), vec!["click(1,1)"]);
}

#[test]
fn wait_own_timeout_is_predicate_not_matched() {
    let mut g = started_a11y(FakePlatform::new(100, 100));
    let wait: WaitForElementArgs =
        serde_json::from_str(r#"{"name":"Missing","timeout_ms":1,"interval_ms":1}"#).unwrap();
    let err = do_actions(&mut g, &do_args(vec![Action::WaitForElement(wait)], 1000)).unwrap_err();
    let envelope: serde_json::Value = serde_json::from_str(&error_text(err)).unwrap();
    assert_eq!(envelope["error"]["code"], "predicate_not_matched");
}

#[test]
fn wait_sequence_timeout_is_sequence_deadline_exceeded() {
    let mut g = started_a11y(FakePlatform::new(100, 100));
    let wait: WaitForElementArgs =
        serde_json::from_str(r#"{"name":"Missing","timeout_ms":1000,"interval_ms":10}"#).unwrap();
    let err = do_actions(&mut g, &do_args(vec![Action::WaitForElement(wait)], 1)).unwrap_err();
    let envelope: serde_json::Value = serde_json::from_str(&error_text(err)).unwrap();
    assert_eq!(envelope["error"]["code"], "sequence_deadline_exceeded");
}

#[test]
fn settle_own_timeout_is_completed_but_sequence_timeout_fails() {
    let black = Frame::solid(100, 100, [0, 0, 0, 255]);
    let white = Frame::solid(100, 100, [255, 255, 255, 255]);
    let settle = Action::Settle(SettleArgs {
        interval_ms: Some(0),
        settle_frames: Some(u32::MAX),
        tolerance: None,
        timeout_ms: Some(2),
        stability_region: None,
        ignore: None,
    });
    let (mut own, _, _) = deadline_glass(
        DeadlineBehavior::Normal,
        vec![black.clone(), white.clone(), black.clone(), white.clone()],
    );
    let out = do_actions(&mut own, &do_args(vec![settle], 1000)).unwrap();
    assert_eq!(
        assert_envelope(&out, "glass_do")["steps"][0]["result"]["settled"],
        false
    );
    let caller_settle = Action::Settle(SettleArgs {
        interval_ms: Some(0),
        settle_frames: Some(u32::MAX),
        tolerance: None,
        timeout_ms: Some(1000),
        stability_region: None,
        ignore: None,
    });
    let (mut caller, _, events) = deadline_glass(
        DeadlineBehavior::Normal,
        vec![black.clone(), white.clone(), black, white],
    );
    let err = do_actions(
        &mut caller,
        &do_args(
            vec![
                click(1, 1),
                caller_settle,
                Action::Key(KeyArgs {
                    chord: "Tab".into(),
                }),
            ],
            1,
        ),
    )
    .unwrap_err();
    let envelope: serde_json::Value = serde_json::from_str(&error_text(err)).unwrap();
    assert_eq!(envelope["error"]["code"], "sequence_deadline_exceeded");
    assert_eq!(envelope["error"]["step"], 1);
    assert_eq!(envelope["outcome"]["executed"], 1);
    assert_eq!(envelope["outcome"]["steps"][0]["status"], "completed");
    let step = &envelope["outcome"]["steps"][1];
    assert_eq!(step["error"]["code"], "sequence_deadline_exceeded");
    assert_eq!(step["attempted"], true);
    assert_eq!(step["side_effects_may_have_occurred"], false);
    assert_eq!(step["result"]["settled"], false);
    assert_eq!(envelope["outcome"]["steps"][2]["status"], "unexecuted");
    assert_eq!(*events.lock().unwrap(), vec!["click(1,1)"]);
}

#[test]
fn caller_soft_return_settle_fails_the_mutating_action() {
    let black = Frame::solid(100, 100, [0, 0, 0, 255]);
    let white = Frame::solid(100, 100, [255, 255, 255, 255]);
    let (mut g, _, events) = deadline_glass(
        DeadlineBehavior::CaptureCompletesLate,
        vec![black.clone(), white.clone(), black, white],
    );
    let err = do_actions(
        &mut g,
        &do_args(
            vec![
                click(1, 1),
                Action::Type(TypeArgs {
                    text: "secret".into(),
                    return_: Some("settle".into()),
                }),
                Action::Key(KeyArgs {
                    chord: "Tab".into(),
                }),
            ],
            150,
        ),
    )
    .unwrap_err();
    let error = error_text(err);
    let envelope: serde_json::Value = serde_json::from_str(&error).unwrap();
    assert_eq!(envelope["error"]["code"], "sequence_deadline_exceeded");
    assert_eq!(envelope["error"]["step"], 1);
    assert_eq!(envelope["outcome"]["executed"], 1);
    let step = &envelope["outcome"]["steps"][1];
    assert_eq!(step["attempted"], true);
    assert_eq!(step["side_effects_may_have_occurred"], true);
    assert_eq!(step["result"]["observed"]["settled"], false);
    assert_eq!(envelope["outcome"]["steps"][2]["status"], "unexecuted");
    assert_eq!(*events.lock().unwrap(), vec!["click(1,1)", "type(secret)"]);
    assert!(!error.contains("secret"));
}

#[test]
fn caller_soft_return_snapshot_stops_before_accessibility_work() {
    let frame = Frame::solid(100, 100, [0, 0, 0, 255]);
    let (mut g, _, events) = deadline_glass(
        DeadlineBehavior::CaptureCompletesLate,
        vec![frame.clone(), frame],
    );
    let err = do_actions(
        &mut g,
        &do_args(
            vec![Action::Type(TypeArgs {
                text: "secret".into(),
                return_: Some("snapshot".into()),
            })],
            150,
        ),
    )
    .unwrap_err();
    let envelope: serde_json::Value = serde_json::from_str(&error_text(err)).unwrap();
    assert_eq!(envelope["error"]["code"], "sequence_deadline_exceeded");
    assert_eq!(envelope["outcome"]["steps"][0]["attempted"], true);
    assert_eq!(
        envelope["outcome"]["steps"][0]["side_effects_may_have_occurred"],
        true
    );
    assert_eq!(*events.lock().unwrap(), vec!["type(secret)"]);
}

#[test]
fn mutating_deadline_failure_warns_side_effects_may_have_occurred() {
    let (mut g, _, events) = deadline_glass(DeadlineBehavior::FailLate, vec![]);
    let err = do_actions(&mut g, &do_args(vec![click(1, 1)], 1)).unwrap_err();
    let envelope: serde_json::Value = serde_json::from_str(&error_text(err)).unwrap();
    let step = &envelope["outcome"]["steps"][0];
    assert_eq!(envelope["error"]["code"], "sequence_deadline_exceeded");
    assert_eq!(step["attempted"], true);
    assert_eq!(step["side_effects_may_have_occurred"], true);
    assert_eq!(envelope["outcome"]["effects_rolled_back"], false);
    assert_eq!(*events.lock().unwrap(), vec!["click(1,1)"]);
}

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
    assert_eq!(error["error"]["code"], "action_failed");
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
    assert_eq!(error["error"]["code"], "action_failed");
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
