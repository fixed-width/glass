use super::testutil::*;
use super::*;
use glass_core::{Backend, BaselineStore, InputMode};
use std::sync::{Arc, Mutex};

fn args<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn background() -> (Glass, tempfile::TempDir, Arc<Mutex<Vec<String>>>) {
    let dir = tempfile::tempdir().unwrap();
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut pending = Some(Backend::display_only(Box::new(
        FakePlatform::new(100, 100).with_event_log(events.clone()),
    )));
    let mut glass = Glass::new_with_input_modes(
        Box::new(move |_, mode| {
            assert_eq!(mode, InputMode::Background);
            Ok(pending.take().unwrap())
        }),
        Box::new(|_, _| Ok(())),
        "fake".into(),
        BaselineStore::new(dir.path()),
        10,
    );
    start(
        &mut glass,
        &args(json!({"run": ["app"], "input_mode": "background", "a11y": false})),
    )
    .unwrap();
    (glass, dir, events)
}

fn error_value(output: &ToolOutput) -> serde_json::Value {
    serde_json::from_str(&output.render_text_blocks()[0]).unwrap()
}

fn assert_mode_refusal(value: &serde_json::Value) {
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "unsupported_input_mode");
    assert_eq!(value["result"]["input_mode"], "background");
    assert_eq!(value["result"]["dispatch"], "not_dispatched");
    assert_eq!(value["result"]["side_effects_may_have_occurred"], false);
    assert_eq!(value["result"]["retry"], "correct_request");
}

#[test]
fn mode_failure_projection_preserves_stronger_dispatch_evidence() {
    let error = InputMode::Background
        .require_foreground("test operation")
        .unwrap_err()
        .after_dispatch();
    let value = input_mode_failure(&error).unwrap();
    assert_eq!(value["dispatch"], "may_have_dispatched");
    assert_eq!(value["side_effects_may_have_occurred"], true);
    assert_eq!(value["retry"], "inspect_before_retry");
}

#[test]
fn foreground_defaults_and_explicit_mode_reach_the_backend_and_start_result() {
    for input_mode in [None, Some("foreground")] {
        let specs: Arc<Mutex<Vec<AppSpec>>> = Arc::default();
        let mut glass = glass_with(FakePlatform::new(100, 100).with_spec_log(specs.clone()));
        let mut value = json!({"run": ["app"], "a11y": false});
        if let Some(mode) = input_mode {
            value["input_mode"] = json!(mode);
        }
        let output = start(&mut glass, &args(value)).unwrap();
        assert_eq!(
            assert_envelope(&output, "glass_start")["input_mode"],
            "foreground"
        );
        assert_eq!(specs.lock().unwrap()[0].input_mode, InputMode::Foreground);
        assert_eq!(glass.active_input_mode(), Some(InputMode::Foreground));
    }
}

#[test]
fn shipped_start_adapter_refuses_background_before_the_factory_and_preserves_session() {
    let specs: Arc<Mutex<Vec<AppSpec>>> = Arc::default();
    let mut glass = glass_with(FakePlatform::new(100, 100).with_spec_log(specs.clone()));
    start(&mut glass, &args(json!({"run": ["app"], "a11y": false}))).unwrap();
    for backend in [
        None,
        Some("macos"),
        Some("x11"),
        Some("wayland"),
        Some("windows"),
        Some("android"),
        Some("ios"),
    ] {
        let mut value = json!({"run": ["app"], "build": "must never run", "input_mode": "background", "a11y": false});
        if let Some(backend) = backend {
            value["backend"] = json!(backend);
        }
        let error = start(&mut glass, &args(value)).unwrap_err();
        assert_mode_refusal(
            &serde_json::from_str(&error)
                .unwrap_or_else(|_| panic!("expected structured mode refusal: {error}")),
        );
        assert_eq!(specs.lock().unwrap().len(), 1);
        assert_eq!(glass.active_input_mode(), Some(InputMode::Foreground));
        assert_eq!(glass.geometry().unwrap().width, 100);
    }
}

#[test]
fn standalone_mutations_preserve_typed_mode_refusal_and_do_not_expose_text() {
    let (mut glass, _dir, events) = background();
    for error in [
        mouse_move(&mut glass, &args(json!({"x": 1, "y": 1}))).unwrap_err(),
        key(&mut glass, &args(json!({"chord": "Return"}))).unwrap_err(),
        click(&mut glass, &args(json!({"x": 1, "y": 1, "count": 2}))).unwrap_err(),
        clipboard_set(&mut glass, &args(json!({"text": "sensitive payload"}))).unwrap_err(),
        window(&mut glass, &args(json!({"op": "focus"}))).unwrap_err(),
        select_window(&mut glass, &args(json!({"id": 9}))).unwrap_err(),
    ] {
        assert_mode_refusal(
            &serde_json::from_str(&error)
                .unwrap_or_else(|_| panic!("expected structured mode refusal: {error}")),
        );
        assert!(!error.contains("sensitive payload"));
    }
    for text in ["", "sensitive payload"] {
        let output = type_text(&mut glass, &args(json!({"text": text}))).unwrap_err();
        assert_mode_refusal(&error_value(&output));
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
        assert_mode_refusal(&error_value(&output));
    }
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn background_batch_preserves_completed_steps_and_stops_at_refusal() {
    let (mut glass, _dir, events) = background();
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
    assert_eq!(value["error"]["code"], "unsupported_input_mode");
    let steps = &value["outcome"]["steps"];
    assert_eq!(steps[0]["status"], "completed");
    assert_eq!(steps[1]["status"], "failed");
    assert_eq!(steps[1]["attempted"], false);
    assert_eq!(steps[1]["side_effects_may_have_occurred"], false);
    assert_eq!(steps[1]["result"]["dispatch"], "not_dispatched");
    assert_eq!(steps[1]["result"]["input_mode"], "background");
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
fn session_capabilities_require_matching_session_and_filter_both_profiles() {
    let mut glass = glass_with(FakePlatform::new(100, 100));
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
        assert_eq!(value["input_mode"], "foreground");
        assert!(value["background_input"]["profile"].is_null());
        for operation in ["click", "scroll", "text"] {
            assert_eq!(
                value["background_input"][operation]["status"],
                "unsupported"
            );
            for tool in value["background_input"][operation]["tools"]
                .as_array()
                .unwrap()
            {
                assert!(profile.includes(tool.as_str().unwrap()));
                assert_ne!(tool, "glass_click_element");
            }
        }
        assert!(
            value["background_input"]["click"]["restriction"]
                .as_str()
                .unwrap()
                .contains("semantic")
        );
    }
    assert_eq!(epoch.generation(), generation);
}
