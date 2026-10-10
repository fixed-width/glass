use super::testutil::*;
use super::*;
use glass_core::{Backend, BaselineStore, InputRoute};
use std::sync::{Arc, Mutex};

fn args<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn targeted() -> (Glass, tempfile::TempDir, Arc<Mutex<Vec<String>>>) {
    let dir = tempfile::tempdir().unwrap();
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut pending = Some(Backend::display_only(Box::new(
        FakePlatform::new(100, 100)
            .with_event_log(events.clone())
            .with_input_route(InputRoute::WindowTargeted),
    )));
    let mut glass = Glass::new(
        Box::new(move |_| Ok(pending.take().unwrap())),
        "fake".into(),
        BaselineStore::new(dir.path()),
        10,
    );
    start(&mut glass, &args(json!({"run": ["app"], "a11y": false}))).unwrap();
    (glass, dir, events)
}

fn error_value(output: &ToolOutput) -> serde_json::Value {
    serde_json::from_str(&output.render_text_blocks()[0]).unwrap()
}

fn assert_operation_refusal(value: &serde_json::Value) {
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "unsupported_operation");
    assert!(value["result"].get("input_mode").is_none());
    assert!(value["result"]["reason"].is_string());
    assert_eq!(value["result"]["dispatch"], "not_dispatched");
    assert_eq!(value["result"]["side_effects_may_have_occurred"], false);
    assert_eq!(value["result"]["retry"], "correct_request");
}

#[test]
fn admission_projection_preserves_stronger_dispatch_evidence() {
    let error = glass_core::GlassError::UnsupportedOperation {
        operation: "test operation",
        reason: "no qualified route",
    }
    .before_dispatch()
    .after_dispatch();
    let value = unsupported_operation_failure(&error).unwrap();
    assert_eq!(value["dispatch"], "may_have_dispatched");
    assert_eq!(value["side_effects_may_have_occurred"], true);
    assert_eq!(value["retry"], "inspect_before_retry");
}

#[test]
fn operation_refusals_keep_their_reason_after_the_sequence_deadline() {
    let context = ToolContext {
        deadline: glass_core::Deadline::at(std::time::Instant::now()),
        owner: Some(glass_core::Whose::Caller),
        allow_wait: false,
    };
    let refused = || {
        glass_core::GlassError::UnsupportedOperation {
            operation: "keyboard or text input",
            reason: "no qualified text route",
        }
        .before_dispatch()
    };
    for error in [
        ContextualError::from_core(refused(), context),
        ContextualError::from_resolved_bound(refused(), context, glass_core::Whose::Caller),
        ContextualError::from_resolved_bound(refused(), context, glass_core::Whose::Callee),
    ] {
        assert_eq!(error.category, SafeErrorCategory::UnsupportedOperation);
        assert!(!error.sequence_deadline_exceeded);
        let result = error.result.unwrap();
        assert_eq!(result["reason"], "no qualified text route");
        assert_eq!(result["dispatch"], "not_dispatched");
    }
    let error = ContextualError::from_core(
        glass_core::GlassError::Backend("transport failed".into()),
        context,
    );
    assert_eq!(error.category, SafeErrorCategory::SequenceDeadlineExceeded);
}

#[test]
fn unchanged_start_result_and_normal_input_need_no_mode_configuration() {
    for route in [InputRoute::Isolated, InputRoute::SharedDesktop] {
        let specs: Arc<Mutex<Vec<AppSpec>>> = Arc::default();
        let mut glass = glass_with(
            FakePlatform::new(100, 100)
                .with_spec_log(specs.clone())
                .with_input_route(route),
        );
        let output = start(&mut glass, &args(json!({"run": ["app"], "a11y": false}))).unwrap();
        let result = assert_envelope(&output, "glass_start");
        assert_eq!(result, json!({"x": 0, "y": 0, "width": 100, "height": 100}));
        assert_eq!(specs.lock().unwrap().len(), 1);
        type_text(&mut glass, &args(json!({"text": "normal typing"}))).unwrap();
    }
}

#[test]
fn standalone_mutations_preserve_typed_operation_refusal_and_do_not_expose_text() {
    let (mut glass, _dir, events) = targeted();
    for error in [
        mouse_move(&mut glass, &args(json!({"x": 1, "y": 1}))).unwrap_err(),
        key(&mut glass, &args(json!({"chord": "Return"}))).unwrap_err(),
        click(&mut glass, &args(json!({"x": 1, "y": 1, "count": 2}))).unwrap_err(),
        clipboard_set(&mut glass, &args(json!({"text": "sensitive payload"}))).unwrap_err(),
        window(&mut glass, &args(json!({"op": "focus"}))).unwrap_err(),
        select_window(&mut glass, &args(json!({"id": 9}))).unwrap_err(),
    ] {
        assert_operation_refusal(
            &serde_json::from_str(&error)
                .unwrap_or_else(|_| panic!("expected structured operation refusal: {error}")),
        );
        assert!(!error.contains("sensitive payload"));
    }
    for text in ["", "sensitive payload"] {
        let output = type_text(&mut glass, &args(json!({"text": text}))).unwrap_err();
        assert_operation_refusal(&error_value(&output));
        assert!(
            !output
                .render_text_blocks()
                .join("")
                .contains("sensitive payload")
        );
    }
    for method in ["auto", "native", "pointer"] {
        let output =
            click_element(&mut glass, &args(json!({"id": 1, "mode": method}))).unwrap_err();
        assert_operation_refusal(&error_value(&output));
    }
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn standalone_window_targeted_scroll_to_element_preserves_operation_refusal() {
    let (mut glass, _dir, events) = targeted();
    let error = scroll_to_element(&mut glass, &args(json!({"name": "Save"}))).unwrap_err();
    let value: serde_json::Value = serde_json::from_str(&error)
        .unwrap_or_else(|_| panic!("expected structured operation refusal: {error}"));
    assert_operation_refusal(&value);
    assert_eq!(value["tool"], "glass_scroll_to_element");
    assert_eq!(value["result"]["operation"], "scroll to element");
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn window_targeted_batch_preserves_completed_steps_and_stops_at_refusal() {
    let (mut glass, _dir, events) = targeted();
    let output = batch::do_actions(
        &mut glass,
        &args(json!({"actions": [
            {"action": "click", "x": 1, "y": 1},
            {"action": "type", "text": "sensitive payload"},
            {"action": "scroll", "x": 1, "y": 1, "dy": 1}
        ]})),
    )
    .unwrap_err();
    let value = error_value(&output);
    assert_eq!(value["error"]["code"], "unsupported_operation");
    let steps = &value["outcome"]["steps"];
    assert_eq!(steps[0]["status"], "completed");
    assert_eq!(steps[1]["status"], "failed");
    assert_eq!(steps[1]["attempted"], false);
    assert_eq!(steps[1]["side_effects_may_have_occurred"], false);
    assert_eq!(steps[1]["result"]["dispatch"], "not_dispatched");
    assert!(steps[1]["result"].get("input_mode").is_none());
    assert_eq!(steps[2]["status"], "unexecuted");
    assert_eq!(events.lock().unwrap().len(), 1);
    assert!(
        !output
            .render_text_blocks()
            .join("")
            .contains("sensitive payload")
    );
}

#[test]
fn session_capabilities_cover_normal_routes_and_filter_both_tool_profiles() {
    for route in [
        InputRoute::Isolated,
        InputRoute::SharedDesktop,
        InputRoute::WindowTargeted,
    ] {
        let mut glass = glass_with(FakePlatform::new(100, 100).with_input_route(route));
        assert!(
            crate::capabilities::render_session_value(&mut glass, None)
                .unwrap_err()
                .contains("no active session")
        );
        start(&mut glass, &args(json!({"run": ["app"], "a11y": false}))).unwrap();
        let epoch = glass.observation_epoch();
        let generation = epoch.generation();
        assert!(crate::capabilities::render_session_value(&mut glass, Some("macos")).is_err());
        for profile in [
            crate::tool_profile::ToolProfile::Full,
            crate::tool_profile::ToolProfile::Lean,
        ] {
            let backend = glass.active_backend().unwrap().to_owned();
            let mut value =
                crate::capabilities::render_session_value(&mut glass, Some(&backend)).unwrap();
            crate::capabilities::apply_tool_profile(&mut value, profile);
            assert_eq!(value["scope"], "session");
            assert!(value.get("input_mode").is_none());
            assert!(value.get("background_input").is_none());
            for operation in ["click", "scroll", "text"] {
                let restricted_text = route == InputRoute::WindowTargeted && operation == "text";
                assert_eq!(
                    value["input"][operation]["status"],
                    if restricted_text {
                        "unsupported"
                    } else {
                        "supported"
                    }
                );
                assert_eq!(
                    value["input"][operation]["desktop_interference"],
                    if restricted_text {
                        "unknown"
                    } else if route == InputRoute::SharedDesktop {
                        "possible"
                    } else {
                        "none"
                    }
                );
                assert_eq!(
                    value["input"][operation].get("restriction").is_some(),
                    route == InputRoute::WindowTargeted && operation != "text"
                );
                for tool in value["input"][operation]["tools"].as_array().unwrap() {
                    assert!(profile.includes(tool.as_str().unwrap()));
                }
            }
        }
        assert_eq!(epoch.generation(), generation);
    }
}
