use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use glass_core::{
    Accessibility, ActionMode, ActionTarget, AppSpec, AxContext, AxNodeId, AxStateCoverage,
    AxTarget, Backend, BaselineStore, BoundDispatch, ChangeSignal, Deadline, Frame, Glass,
    GlassError, HostPathProtectionMode, KeyEvent, Platform, PlatformFactory, PointerEvent,
    PointerHit, ProtectedHostPath, Region, SandboxLevel, Stream, WindowGeometry, WindowId,
    WindowInfo, WindowOp,
};

use super::{
    ValidatedType, validate_action, validate_click_element_args, validate_set_value_args,
    validate_type_args,
};
use crate::params::{Action, ClickElementArgs, SetValueArgs, TypeArgs};
use crate::tools::{
    ContextualError, ToolContext, click_element_with, set_value_with, type_text_with,
};

#[derive(Clone, Copy)]
enum SessionIoKind {
    Action,
    Input,
    Capture,
    Accessibility,
    Other,
}

#[derive(Default)]
struct SessionIoCounters {
    action: AtomicUsize,
    input: AtomicUsize,
    capture: AtomicUsize,
    accessibility: AtomicUsize,
    other: AtomicUsize,
    calls: Mutex<Vec<&'static str>>,
}

impl SessionIoCounters {
    fn record(&self, kind: SessionIoKind, call: &'static str) {
        let counter = match kind {
            SessionIoKind::Action => &self.action,
            SessionIoKind::Input => &self.input,
            SessionIoKind::Capture => &self.capture,
            SessionIoKind::Accessibility => &self.accessibility,
            SessionIoKind::Other => &self.other,
        };
        counter.fetch_add(1, Ordering::SeqCst);
        self.calls.lock().unwrap().push(call);
    }

    fn clear(&self) {
        self.action.store(0, Ordering::SeqCst);
        self.input.store(0, Ordering::SeqCst);
        self.capture.store(0, Ordering::SeqCst);
        self.accessibility.store(0, Ordering::SeqCst);
        self.other.store(0, Ordering::SeqCst);
        self.calls.lock().unwrap().clear();
    }

    fn assert_zero(&self, name: &str) {
        let counts = [
            ("action", self.action.load(Ordering::SeqCst)),
            ("input", self.input.load(Ordering::SeqCst)),
            ("capture", self.capture.load(Ordering::SeqCst)),
            ("accessibility", self.accessibility.load(Ordering::SeqCst)),
            ("other", self.other.load(Ordering::SeqCst)),
        ];
        assert!(
            counts.iter().all(|(_, count)| *count == 0),
            "{name}: invalid request performed post-start session I/O: {counts:?}; calls={:?}",
            self.calls.lock().unwrap()
        );
    }
}

struct InstrumentedPlatform {
    counters: Arc<SessionIoCounters>,
    geometry: WindowGeometry,
}

impl Platform for InstrumentedPlatform {
    fn configure_protected_host_paths(
        &mut self,
        _paths: &[ProtectedHostPath],
    ) -> glass_core::Result<HostPathProtectionMode> {
        self.counters.record(SessionIoKind::Other, "configure");
        Ok(HostPathProtectionMode::SandboxRules)
    }

    fn start_app(&mut self, _spec: &AppSpec) -> glass_core::Result<WindowGeometry> {
        self.counters.record(SessionIoKind::Other, "start");
        Ok(self.geometry.clone())
    }

    fn stop_app_by(&mut self, _deadline: Deadline) -> glass_core::Result<()> {
        self.counters.record(SessionIoKind::Other, "stop");
        Ok(())
    }

    fn capture_frame_by(
        &mut self,
        _region: Option<&Region>,
        _deadline: Deadline,
    ) -> glass_core::Result<Frame> {
        self.counters
            .record(SessionIoKind::Capture, "capture_frame");
        Frame::new(1, 1, vec![0, 0, 0, 255])
    }

    fn capture_window_by(
        &mut self,
        _id: WindowId,
        _region: Option<&Region>,
        _deadline: Deadline,
    ) -> glass_core::Result<Frame> {
        self.counters
            .record(SessionIoKind::Capture, "capture_window");
        Frame::new(1, 1, vec![0, 0, 0, 255])
    }

    fn send_pointer_by(
        &mut self,
        _event: &PointerEvent,
        _deadline: Deadline,
    ) -> glass_core::Result<()> {
        self.counters.record(SessionIoKind::Input, "pointer");
        Ok(())
    }

    fn send_key_by(&mut self, _event: &KeyEvent, _deadline: Deadline) -> glass_core::Result<()> {
        self.counters.record(SessionIoKind::Input, "key");
        Ok(())
    }

    fn get_clipboard(&mut self) -> glass_core::Result<String> {
        self.counters.record(SessionIoKind::Other, "get_clipboard");
        Ok(String::new())
    }

    fn set_clipboard(&mut self, _text: &str) -> glass_core::Result<()> {
        self.counters.record(SessionIoKind::Other, "set_clipboard");
        Ok(())
    }

    fn window_by(
        &mut self,
        _op: &WindowOp,
        _deadline: Deadline,
    ) -> glass_core::Result<WindowGeometry> {
        self.counters.record(SessionIoKind::Action, "window");
        Ok(self.geometry.clone())
    }

    fn list_windows_by(&mut self, _deadline: Deadline) -> glass_core::Result<Vec<WindowInfo>> {
        self.counters.record(SessionIoKind::Other, "list_windows");
        Ok(vec![WindowInfo {
            id: WindowId(1),
            title: Some("instrumented".into()),
            class: None,
            geometry: self.geometry.clone(),
            active: true,
        }])
    }

    fn select_window_by(
        &mut self,
        _id: WindowId,
        _deadline: Deadline,
    ) -> glass_core::Result<WindowGeometry> {
        self.counters.record(SessionIoKind::Action, "select_window");
        Ok(self.geometry.clone())
    }

    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        self.counters.record(SessionIoKind::Other, "drain_logs");
        Vec::new()
    }

    fn app_pid(&self) -> Option<u32> {
        self.counters.record(SessionIoKind::Other, "app_pid");
        None
    }

    fn app_pids(&self) -> Vec<u32> {
        self.counters.record(SessionIoKind::Other, "app_pids");
        Vec::new()
    }

    fn app_pids_by(&self, _deadline: Deadline) -> glass_core::Result<Vec<u32>> {
        self.counters.record(SessionIoKind::Other, "app_pids_by");
        Ok(Vec::new())
    }

    fn a11y_bus_addr(&self) -> Option<String> {
        self.counters.record(SessionIoKind::Other, "a11y_bus_addr");
        None
    }

    fn active_window_handle(&self) -> Option<i64> {
        self.counters
            .record(SessionIoKind::Other, "active_window_handle");
        None
    }

    fn a11y_toggle_control_at_trailing_edge(&self) -> bool {
        self.counters
            .record(SessionIoKind::Other, "a11y_toggle_control_at_trailing_edge");
        false
    }
}

struct InstrumentedAccessibility {
    counters: Arc<SessionIoCounters>,
}

impl Accessibility for InstrumentedAccessibility {
    fn snapshot(&mut self, _ctx: &AxContext) -> glass_core::Result<glass_core::AxTree> {
        self.counters
            .record(SessionIoKind::Accessibility, "a11y_snapshot");
        Ok(crate::tools::testutil::empty_tree())
    }

    fn subscribe_changes(&mut self, _ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
        self.counters
            .record(SessionIoKind::Accessibility, "a11y_subscribe");
        None
    }

    fn state_coverage(&self) -> AxStateCoverage {
        self.counters
            .record(SessionIoKind::Accessibility, "a11y_state_coverage");
        AxStateCoverage::NONE
    }

    fn focus(
        &mut self,
        _ctx: &AxContext,
        _target: &AxTarget,
    ) -> glass_core::Result<Option<AxNodeId>> {
        self.counters.record(SessionIoKind::Action, "a11y_focus");
        Ok(None)
    }

    fn pointer_target_at(
        &mut self,
        _ctx: &AxContext,
        _target: &AxTarget,
        _point: (i32, i32),
    ) -> glass_core::Result<PointerHit> {
        self.counters
            .record(SessionIoKind::Accessibility, "a11y_pointer_target");
        Ok(PointerHit::Inconclusive)
    }

    fn set_value(
        &mut self,
        _ctx: &AxContext,
        _target: &AxTarget,
        _text: &str,
    ) -> glass_core::Result<()> {
        self.counters
            .record(SessionIoKind::Action, "a11y_set_value");
        Ok(())
    }

    fn invoke(
        &mut self,
        _ctx: &AxContext,
        _target: &AxTarget,
    ) -> glass_core::Result<Option<AxNodeId>> {
        self.counters.record(SessionIoKind::Action, "a11y_invoke");
        Ok(None)
    }
}

fn started_instrumented_glass() -> (Glass, Arc<SessionIoCounters>, Arc<AtomicUsize>) {
    let counters = Arc::new(SessionIoCounters::default());
    let geometry = WindowGeometry {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    let mut held = Some(Backend {
        platform: Box::new(InstrumentedPlatform {
            counters: counters.clone(),
            geometry,
        }),
        accessibility: Some(Box::new(InstrumentedAccessibility {
            counters: counters.clone(),
        })),
    });
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let recorded_factory_calls = factory_calls.clone();
    let factory: PlatformFactory = Box::new(move |_| {
        recorded_factory_calls.fetch_add(1, Ordering::SeqCst);
        held.take()
            .ok_or_else(|| GlassError::Backend("test factory called twice".into()))
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    let mut glass = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["app".into()],
            cwd: None,
            env: Vec::new(),
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
            a11y: true,
        })
        .unwrap();
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
    counters.clear();
    (glass, counters, factory_calls)
}

#[test]
fn invalid_handler_harness_runs_with_an_active_session() {
    let (glass, counters, factory_calls) = started_instrumented_glass();
    assert!(
        glass.geometry().is_ok(),
        "invalid-handler tests must exercise validation against an active session"
    );
    counters.assert_zero("harness setup");
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_handler_harness_observes_every_session_io_category() {
    let (mut glass, counters, _) = started_instrumented_glass();
    glass.screenshot(None, None).unwrap();
    glass.key(&KeyEvent::Text("x".into())).unwrap();
    glass.a11y_snapshot(None).unwrap();
    glass.window(&WindowOp::Geometry).unwrap();

    for (kind, count) in [
        ("action", counters.action.load(Ordering::SeqCst)),
        ("input", counters.input.load(Ordering::SeqCst)),
        ("capture", counters.capture.load(Ordering::SeqCst)),
        (
            "accessibility",
            counters.accessibility.load(Ordering::SeqCst),
        ),
        ("other", counters.other.load(Ordering::SeqCst)),
    ] {
        assert!(count > 0, "instrumentation did not observe {kind} I/O");
    }
}

enum InvalidHandlerArgs {
    Click(ClickElementArgs),
    SetValue(SetValueArgs),
    Type(TypeArgs),
}

fn assert_invalid_handler(name: &str, args: InvalidHandlerArgs, expected: &str) {
    let (mut glass, counters, factory_calls) = started_instrumented_glass();
    let result = match args {
        InvalidHandlerArgs::Click(args) => {
            click_element_with(&mut glass, &args, ToolContext::UNBOUNDED)
        }
        InvalidHandlerArgs::SetValue(args) => {
            set_value_with(&mut glass, &args, ToolContext::UNBOUNDED)
        }
        InvalidHandlerArgs::Type(args) => type_text_with(&mut glass, &args, ToolContext::UNBOUNDED),
    };
    let error = result.unwrap_err();
    assert!(
        error.message.contains(expected),
        "{name}: expected {expected:?}, got {:?}",
        error.message
    );
    assert_eq!(
        error.bound_dispatch,
        Some(BoundDispatch::NotDispatched),
        "{name}: validation must be proven pre-dispatch"
    );
    counters.assert_zero(name);
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        1,
        "{name}: validation attempted to create another session"
    );
}

#[test]
fn invalid_semantic_handler_arguments_fail_before_session_io() {
    let click =
        |json| InvalidHandlerArgs::Click(serde_json::from_str::<ClickElementArgs>(json).unwrap());
    let set_value =
        |json| InvalidHandlerArgs::SetValue(serde_json::from_str::<SetValueArgs>(json).unwrap());
    let type_text =
        |json| InvalidHandlerArgs::Type(serde_json::from_str::<TypeArgs>(json).unwrap());

    let rows = [
        ("click neither", click(r#"{}"#), "exactly one"),
        (
            "click both",
            click(r#"{"id":1,"target":{"role":"Button"}}"#),
            "exactly one",
        ),
        (
            "click id timeout",
            click(r#"{"id":1,"timeout_ms":1}"#),
            "timeout_ms",
        ),
        (
            "click id max nodes",
            click(r#"{"id":1,"max_nodes":0}"#),
            "max_nodes",
        ),
        (
            "click selector timeout too large",
            click(r#"{"target":{"role":"Button"},"timeout_ms":120001}"#),
            "120000",
        ),
        (
            "click empty target",
            click(r#"{"target":{}}"#),
            "specify query",
        ),
        (
            "click unknown role",
            click(r#"{"target":{"role":"Mystery"}}"#),
            "unknown role",
        ),
        (
            "click unknown state",
            click(r#"{"target":{"states":["sparkling"]}}"#),
            "unknown state",
        ),
        (
            "click contradictory states",
            click(r#"{"target":{"states":["enabled","disabled"]}}"#),
            "contradict",
        ),
        (
            "click unknown return",
            click(r#"{"id":1,"return":"later"}"#),
            "unknown return",
        ),
        ("set neither", set_value(r#"{"text":"Ada"}"#), "exactly one"),
        (
            "set both",
            set_value(r#"{"id":1,"target":{"role":"TextField"},"text":"Ada"}"#),
            "exactly one",
        ),
        (
            "set id timeout",
            set_value(r#"{"id":1,"text":"Ada","timeout_ms":1}"#),
            "timeout_ms",
        ),
        (
            "set id max nodes",
            set_value(r#"{"id":1,"text":"Ada","max_nodes":0}"#),
            "max_nodes",
        ),
        (
            "set unknown role",
            set_value(r#"{"target":{"role":"Mystery"},"text":"Ada"}"#),
            "unknown role",
        ),
        (
            "set unknown return",
            set_value(r#"{"id":1,"text":"Ada","return":"later"}"#),
            "unknown return",
        ),
        (
            "untargeted type focus mode",
            type_text(r#"{"text":"Ada","focus_mode":"auto"}"#),
            "focus_mode",
        ),
        (
            "untargeted type timeout",
            type_text(r#"{"text":"Ada","timeout_ms":1}"#),
            "timeout_ms",
        ),
        (
            "untargeted type max nodes",
            type_text(r#"{"text":"Ada","max_nodes":0}"#),
            "max_nodes",
        ),
        (
            "targeted type timeout too large",
            type_text(r#"{"target":{"role":"TextField"},"text":"Ada","timeout_ms":120001}"#),
            "120000",
        ),
        (
            "targeted type unknown state",
            type_text(r#"{"target":{"states":["sparkling"]},"text":"Ada"}"#),
            "unknown state",
        ),
        (
            "type unknown return",
            type_text(r#"{"text":"Ada","return":"later"}"#),
            "unknown return",
        ),
    ];

    for (name, args, expected) in rows {
        assert_invalid_handler(name, args, expected);
    }
}

#[test]
fn invalid_typed_modes_and_recursive_or_misspelled_scopes_fail_deserialization() {
    assert!(serde_json::from_str::<ClickElementArgs>(r#"{"id":1,"mode":"magic"}"#).is_err());
    assert!(
        serde_json::from_str::<TypeArgs>(
            r#"{"target":{"role":"TextField"},"text":"Ada","focus_mode":"magic"}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<Action>(r#"{"action":"click_element","id":1,"mode":"magic"}"#)
            .is_err()
    );
    assert!(
        serde_json::from_str::<Action>(
            r#"{"action":"type","target":{"role":"TextField"},"text":"Ada","focus_mode":"magic"}"#
        )
        .is_err()
    );

    for json in [
        r#"{"target":{"role":"Button","within":{"role":"Document","within":{"role":"Group"}}}}"#,
        r#"{"target":{"role":"Button","statse":["enabled"]}}"#,
        r#"{"target":{"role":"Button","within":{"rol":"Document"}}}"#,
    ] {
        assert!(
            serde_json::from_str::<ClickElementArgs>(json).is_err(),
            "recursive or misspelled scope unexpectedly parsed: {json}"
        );
    }
}

#[test]
fn valid_target_forms_bind_legacy_and_semantic_defaults() {
    let legacy: ClickElementArgs = serde_json::from_str(r#"{"id":42,"mode":"pointer"}"#).unwrap();
    let legacy = validate_click_element_args(&legacy).unwrap();
    assert!(matches!(legacy.target, ActionTarget::Id(id) if id.0 == 42));
    assert_eq!(legacy.mode, ActionMode::Pointer);
    assert_eq!(legacy.timeout_ms, None);
    assert_eq!(legacy.max_nodes, None);

    let semantic: ClickElementArgs =
        serde_json::from_str(r#"{"target":{"role":"Button"}}"#).unwrap();
    let semantic = validate_click_element_args(&semantic).unwrap();
    assert!(matches!(semantic.target, ActionTarget::Semantic(_)));
    assert_eq!(semantic.mode, ActionMode::Auto);
    assert_eq!(semantic.timeout_ms, Some(10_000));
    assert_eq!(semantic.max_nodes, None);

    let zero: SetValueArgs =
        serde_json::from_str(r#"{"target":{"role":"TextField"},"text":"Ada","timeout_ms":0}"#)
            .unwrap();
    let zero = validate_set_value_args(&zero).unwrap();
    assert_eq!(zero.timeout_ms, Some(0));
    assert_eq!(zero.max_nodes, None);

    let targeted: TypeArgs =
        serde_json::from_str(r#"{"target":{"role":"TextField"},"text":"Ada"}"#).unwrap();
    let ValidatedType::Targeted(targeted) = validate_type_args(&targeted).unwrap() else {
        panic!("targeted type must bind targeted params")
    };
    assert_eq!(targeted.focus_mode, ActionMode::Auto);
    assert_eq!(targeted.timeout_ms, 10_000);
    assert_eq!(targeted.max_nodes, None);

    let untargeted: TypeArgs =
        serde_json::from_str(r#"{"text":"Ada","return":"snapshot"}"#).unwrap();
    assert!(matches!(
        validate_type_args(&untargeted).unwrap(),
        ValidatedType::Untargeted
    ));
}

fn action(json: &str) -> Action {
    serde_json::from_str(json).unwrap()
}

#[test]
fn validate_action_accepts_every_variant_without_session_state() {
    for action in [
        action(r#"{"action":"click","x":1,"y":2}"#),
        action(r#"{"action":"move","x":1,"y":2}"#),
        action(r#"{"action":"drag","x1":1,"y1":2,"x2":3,"y2":4}"#),
        action(r#"{"action":"scroll","x":1,"y":2}"#),
        action(r#"{"action":"type","text":"Ada"}"#),
        action(r#"{"action":"key","chord":"Return"}"#),
        action(r#"{"action":"settle"}"#),
        action(r#"{"action":"click_element","id":1}"#),
        action(r#"{"action":"set_value","id":1,"text":"Ada"}"#),
        action(r#"{"action":"wait_for_element","role":"Button"}"#),
        action(r#"{"action":"scroll_to_element","role":"Button"}"#),
    ] {
        validate_action(&action).unwrap();
    }
}

#[test]
fn validate_action_routes_each_invalid_variant_to_its_pure_helper() {
    let rows = [
        (
            action(r#"{"action":"click","x":1,"y":2,"count":0}"#),
            "count",
        ),
        (
            action(r#"{"action":"drag","x1":1,"y1":2,"x2":3,"y2":4,"button":"bad"}"#),
            "button",
        ),
        (
            action(r#"{"action":"scroll","x":1,"y":2,"dx":101}"#),
            "between",
        ),
        (
            action(r#"{"action":"type","text":"Ada","focus_mode":"auto"}"#),
            "focus_mode",
        ),
        (action(r#"{"action":"key","chord":"ctrl+"}"#), "key"),
        (
            action(r#"{"action":"settle","interval_ms":0}"#),
            "interval_ms",
        ),
        (
            action(r#"{"action":"settle","stability_region":{"x":0,"y":0,"width":0,"height":1}}"#),
            "stability_region",
        ),
        (
            action(r#"{"action":"settle","ignore":[{"x":0,"y":0,"width":1,"height":0}]}"#),
            "ignore",
        ),
        (action(r#"{"action":"click_element"}"#), "exactly one"),
        (
            action(r#"{"action":"set_value","text":"Ada"}"#),
            "exactly one",
        ),
        (
            action(r#"{"action":"wait_for_element","role":"Mystery"}"#),
            "unknown role",
        ),
        (
            action(r#"{"action":"scroll_to_element","role":"Button","x":1}"#),
            "both",
        ),
    ];
    for (action, expected) in rows {
        let error: ContextualError = validate_action(&action).unwrap_err();
        assert!(
            error.message.contains(expected),
            "expected {expected:?}, got {:?}",
            error.message
        );
        assert_eq!(error.bound_dispatch, Some(BoundDispatch::NotDispatched));
    }
}
