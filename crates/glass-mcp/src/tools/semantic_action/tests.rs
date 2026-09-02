use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use glass_core::{
    Accessibility, ActionDeadline, ActionMethod, ActionMode, ActionTarget, ActionabilityCheck,
    ActionabilityCheckName, ActionabilityReport, ActionabilitySource, ActionabilityVerdict,
    AppSpec, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStateCoverage, AxStates, AxTarget,
    Backend, BaselineStore, BoundDispatch, ChangeSignal, ConfirmationStatus, Deadline,
    DispatchStatus, ElementInfo, Frame, Glass, GlassError, HostPathProtectionMode, KeyEvent,
    MatchField, MatchTier, MutationReport, Platform, PlatformFactory, PointerEvent, PointerHit,
    ProtectedHostPath, Region, ResolutionReport, RetryGuidance, SandboxLevel, ScopeResolution,
    SemanticActionError, SemanticActionFailureKind, SemanticActionOutcome, SemanticMatch, Stream,
    Whose, WindowGeometry, WindowId, WindowInfo, WindowOp,
};

use super::{
    ValidatedType, candidates_json, element_json, semantic_error, success_output, validate_action,
    validate_click_element_args, validate_set_value_args, validate_type_args,
};
use crate::params::{Action, ClickElementArgs, DoArgs, SetValueArgs, TypeArgs};
use crate::tools::{
    ContextualError, SafeErrorCategory, ToolContext, click_element_with, do_actions,
    erase_semantic_context, set_value, set_value_with, type_text, type_text_with,
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
    key_error: Option<String>,
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
        match &self.key_error {
            Some(message) => Err(GlassError::Backend(message.clone())),
            None => Ok(()),
        }
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
    tree: glass_core::AxTree,
    coverage: AxStateCoverage,
    invoke_result: Option<AxNodeId>,
    set_error: Option<String>,
}

impl Accessibility for InstrumentedAccessibility {
    fn snapshot(&mut self, _ctx: &AxContext) -> glass_core::Result<glass_core::AxTree> {
        self.counters
            .record(SessionIoKind::Accessibility, "a11y_snapshot");
        Ok(self.tree.clone())
    }

    fn subscribe_changes(&mut self, _ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
        self.counters
            .record(SessionIoKind::Accessibility, "a11y_subscribe");
        None
    }

    fn state_coverage(&self) -> AxStateCoverage {
        self.counters
            .record(SessionIoKind::Accessibility, "a11y_state_coverage");
        self.coverage
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
        match &self.set_error {
            Some(message) => Err(GlassError::Backend(message.clone())),
            None => Ok(()),
        }
    }

    fn invoke(
        &mut self,
        _ctx: &AxContext,
        _target: &AxTarget,
    ) -> glass_core::Result<Option<AxNodeId>> {
        self.counters.record(SessionIoKind::Action, "a11y_invoke");
        Ok(self.invoke_result)
    }
}

fn started_instrumented_glass() -> (Glass, Arc<SessionIoCounters>, Arc<AtomicUsize>) {
    started_instrumented_glass_with(
        crate::tools::testutil::empty_tree(),
        AxStateCoverage::NONE,
        None,
    )
}

fn started_instrumented_glass_with(
    tree: glass_core::AxTree,
    coverage: AxStateCoverage,
    invoke_result: Option<AxNodeId>,
) -> (Glass, Arc<SessionIoCounters>, Arc<AtomicUsize>) {
    started_instrumented_glass_with_errors(tree, coverage, invoke_result, None, None)
}

fn started_instrumented_glass_with_errors(
    tree: glass_core::AxTree,
    coverage: AxStateCoverage,
    invoke_result: Option<AxNodeId>,
    key_error: Option<String>,
    set_error: Option<String>,
) -> (Glass, Arc<SessionIoCounters>, Arc<AtomicUsize>) {
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
            key_error,
        }),
        accessibility: Some(Box::new(InstrumentedAccessibility {
            counters: counters.clone(),
            tree,
            coverage,
            invoke_result,
            set_error,
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

fn element_with_text(name: &str, value: &str, secure: bool) -> ElementInfo {
    ElementInfo {
        id: AxNodeId(4),
        role: AxRole::Button,
        name: Some(name.into()),
        description: Some(format!("description for {name}")),
        value: Some(value.into()),
        bounds: Some(AxRect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        }),
        states: AxStates {
            enabled: true,
            visible: true,
            secure,
            ..AxStates::default()
        },
    }
}

fn actionability_report() -> ActionabilityReport {
    ActionabilityReport {
        checks: vec![ActionabilityCheck::new(
            ActionabilityCheckName::Unique,
            ActionabilityVerdict::Passed,
            true,
            ActionabilitySource::SemanticResolution,
        )],
    }
}

fn resolution_report(timed_out_by: Option<Whose>) -> ResolutionReport {
    ResolutionReport {
        elapsed_ms: 12,
        scope: ScopeResolution::Resolved(AxNodeId(3)),
        matches_in_walk: 1,
        search_complete: true,
        timed_out_by,
        tree_truncated: false,
        unreadable_subtrees: 0,
        unexposed_placeholders: 0,
    }
}

fn action_bound(owner: Option<Whose>) -> ActionDeadline {
    ActionDeadline {
        deadline: Deadline::UNBOUNDED,
        owner,
        allow_wait: true,
    }
}

fn outcome(
    method: ActionMethod,
    focus: Option<MutationReport>,
    semantic: bool,
    name: &str,
) -> SemanticActionOutcome {
    SemanticActionOutcome {
        target: element_with_text(name, "application value", false),
        resolution: semantic.then(|| resolution_report(None)),
        actionability: actionability_report(),
        focus,
        action: MutationReport {
            method,
            dispatch: DispatchStatus::Dispatched,
            confirmation: ConfirmationStatus::DispatchConfirmed,
        },
        bound: action_bound(Some(Whose::Callee)),
    }
}

fn semantic_match(name: &str, context: &str) -> SemanticMatch {
    SemanticMatch {
        element: element_with_text(name, "candidate value", false),
        field: Some(MatchField::Name),
        tier: MatchTier::ExactName,
        context: context.into(),
    }
}

fn semantic_failure(
    kind: SemanticActionFailureKind,
    timed_out_by: Option<Whose>,
    owner: Option<Whose>,
    candidates: Vec<SemanticMatch>,
) -> SemanticActionError {
    SemanticActionError {
        kind,
        summary: "APP CONTROLLED SUMMARY MUST NOT BE USED",
        resolution: Some(resolution_report(timed_out_by)),
        actionability: actionability_report(),
        focus: None,
        action_dispatch: DispatchStatus::NotDispatched,
        candidates,
        target: None,
        bound: action_bound(owner),
        retry: RetryGuidance::WaitOrRefine,
        source: Some(GlassError::Backend(
            "APP CONTROLLED BACKEND DETAIL MUST NOT BE USED".into(),
        )),
    }
}

fn success_result(output: &crate::tools::ToolOutput) -> &serde_json::Value {
    let crate::tools::OutContent::Envelope(envelope) = &output.0[0] else {
        panic!("expected success envelope")
    };
    &envelope.result
}

fn error_envelope(output: &crate::tools::ToolOutput) -> serde_json::Value {
    let block = output.render_text_blocks().remove(0);
    serde_json::from_str(&block).expect("structured error envelope")
}

fn untrusted_body(block: &str) -> &str {
    let after_open = block.split_once("⟧\n").expect("untrusted open marker").1;
    after_open
        .rsplit_once("\n⟦/untrusted:")
        .expect("untrusted close marker")
        .0
}

#[test]
fn semantic_success_shapes_disclose_mutation_and_complete_evidence() {
    let click = success_output(
        "glass_click_element",
        &outcome(
            ActionMethod::NativeAction {
                actuated: Some(AxNodeId(9)),
            },
            None,
            true,
            "Save",
        ),
        None,
        vec![],
    );
    let click_result = success_result(&click);
    assert_eq!(click_result["id"], 4);
    assert_eq!(click_result["method"], "native-action");
    assert_eq!(click_result["dispatch"], "dispatched");
    assert_eq!(click_result["confirmation"], "dispatch_confirmed");
    assert_eq!(click_result["actuated_id"], 9);
    assert_eq!(click_result["resolution"]["source"], "semantic");
    assert_eq!(click_result["resolution"]["elapsed_ms"], 12);
    assert_eq!(click_result["resolution"]["scope_status"], "resolved");
    assert_eq!(click_result["resolution"]["resolved_scope_id"], 3);
    assert_eq!(click_result["resolution"]["search_complete"], true);
    assert_eq!(click_result["resolution"]["tree_truncated"], false);
    assert_eq!(
        click_result["actionability"][0],
        serde_json::json!({
            "check": "unique",
            "verdict": "passed",
            "required": true,
            "source": "semantic_resolution",
        })
    );

    let set = success_output(
        "glass_set_value",
        &outcome(ActionMethod::AccessibilityValue, None, true, "Account name"),
        Some(serde_json::json!({"settled": true})),
        vec![],
    );
    let set_result = success_result(&set);
    assert_eq!(set_result["method"], "accessibility-value");
    assert_eq!(set_result["dispatch"], "dispatched");
    assert_eq!(set_result["confirmation"], "dispatch_confirmed");
    assert_eq!(set_result["observed"]["settled"], true);

    let typed = success_output(
        "glass_type",
        &outcome(
            ActionMethod::Keyboard,
            Some(MutationReport {
                method: ActionMethod::NativeAction { actuated: None },
                dispatch: DispatchStatus::Dispatched,
                confirmation: ConfirmationStatus::FocusConfirmed,
            }),
            true,
            "UNIQUE_SUBMITTED_TYPE_SENTINEL",
        ),
        None,
        vec![],
    );
    let type_result = success_result(&typed);
    assert_eq!(type_result["focus_method"], "native-action");
    assert_eq!(type_result["focus_dispatch"], "dispatched");
    assert_eq!(type_result["focus_confirmation"], "focus_confirmed");
    assert_eq!(type_result["type_dispatch"], "dispatched");
    assert!(type_result.get("method").is_none());
    assert!(
        typed
            .render_text_blocks()
            .iter()
            .all(|block| !block.contains("UNIQUE_SUBMITTED_TYPE_SENTINEL"))
    );
}

#[test]
fn semantic_selector_success_has_one_untrusted_target_sibling_and_trusted_text_is_clean() {
    let output = success_output(
        "glass_click_element",
        &outcome(
            ActionMethod::Pointer {
                native_fallback: None,
            },
            None,
            true,
            "Pay now",
        ),
        None,
        vec![],
    );
    assert_eq!(output.0.len(), 2);
    let trusted = output.render_text_blocks()[0].clone();
    assert!(!trusted.contains("Pay now"));
    assert!(!trusted.contains("description for"));
    assert!(!trusted.contains("application value"));
    let sibling = output.text_block(1).expect("target sibling");
    assert_eq!(
        sibling.trust,
        crate::output::TextTrust::UntrustedApplication
    );
    let body: serde_json::Value =
        serde_json::from_str(untrusted_body(&sibling.body)).expect("target JSON");
    assert_eq!(body["target"]["name"], "Pay now");
    assert_eq!(
        success_result(&output)["content_blocks"],
        serde_json::json!([1])
    );

    let legacy = success_output(
        "glass_click_element",
        &outcome(
            ActionMethod::Pointer {
                native_fallback: None,
            },
            None,
            false,
            "Pay now",
        ),
        None,
        vec![],
    );
    assert_eq!(legacy.0.len(), 1);
    assert!(success_result(&legacy).get("resolution").is_none());
    assert!(success_result(&legacy).get("content_blocks").is_none());
}

#[test]
fn element_and_candidate_rendering_enforce_text_and_secure_value_policy() {
    let secure = element_with_text("Secret field", "TOP SECRET", true);
    let visible = element_json(&secure, true);
    assert_eq!(visible["name"], "Secret field");
    assert_eq!(visible["value"], serde_json::Value::Null);

    let hidden = element_json(&secure, false);
    assert_eq!(hidden["name"], serde_json::Value::Null);
    assert_eq!(hidden["description"], serde_json::Value::Null);
    assert_eq!(hidden["value"], serde_json::Value::Null);

    let candidates = candidates_json(&[semantic_match("Neighbor", "near Account")], false);
    assert_eq!(candidates[0]["name"], serde_json::Value::Null);
    assert_eq!(candidates[0]["matched_text"], serde_json::Value::Null);
    assert_eq!(candidates[0]["context"], serde_json::Value::Null);
    assert_eq!(candidates[0]["id"], 4);
    assert_eq!(candidates[0]["match_tier"], "exact_name");
}

#[test]
fn semantic_error_categories_and_codes_are_stable_snake_case() {
    let rows = [
        (
            SemanticActionFailureKind::NoMatch,
            SafeErrorCategory::NoMatch,
            "no_match",
        ),
        (
            SemanticActionFailureKind::AmbiguousTarget,
            SafeErrorCategory::AmbiguousTarget,
            "ambiguous_target",
        ),
        (
            SemanticActionFailureKind::AmbiguousScope,
            SafeErrorCategory::AmbiguousScope,
            "ambiguous_scope",
        ),
        (
            SemanticActionFailureKind::IncompleteTree,
            SafeErrorCategory::IncompleteTree,
            "incomplete_tree",
        ),
        (
            SemanticActionFailureKind::UnprovenSelectorState,
            SafeErrorCategory::UnprovenSelectorState,
            "unproven_selector_state",
        ),
        (
            SemanticActionFailureKind::NotActionable,
            SafeErrorCategory::NotActionable,
            "not_actionable",
        ),
        (
            SemanticActionFailureKind::UnstableTarget,
            SafeErrorCategory::UnstableTarget,
            "unstable_target",
        ),
        (
            SemanticActionFailureKind::FocusUnconfirmed,
            SafeErrorCategory::FocusUnconfirmed,
            "focus_unconfirmed",
        ),
        (
            SemanticActionFailureKind::UnsupportedMode,
            SafeErrorCategory::UnsupportedMode,
            "unsupported_mode",
        ),
    ];
    for (kind, category, code) in rows {
        let rendered = semantic_error(
            "glass_click_element",
            semantic_failure(kind, None, None, vec![]),
        );
        assert_eq!(rendered.category, category);
        assert_eq!(rendered.code, code);
        assert_eq!(serde_json::to_value(category).unwrap(), code);
        assert!(!rendered.message.contains("APP CONTROLLED"));
    }
}

#[test]
fn structured_ambiguity_error_keeps_candidates_only_in_untrusted_sibling() {
    let error = semantic_error(
        "glass_click_element",
        semantic_failure(
            SemanticActionFailureKind::AmbiguousTarget,
            None,
            None,
            vec![
                semantic_match("Pay personal", "inside Personal card"),
                semantic_match("Pay business", "inside Business card"),
            ],
        ),
    );
    let output = erase_semantic_context("glass_click_element", Err(error)).unwrap_err();
    let envelope = error_envelope(&output);
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["tool"], "glass_click_element");
    assert_eq!(envelope["error"]["code"], "ambiguous_target");
    assert_eq!(envelope["error"]["summary"], "semantic target is ambiguous");
    assert_eq!(envelope["result"]["dispatch"], "not_dispatched");
    assert_eq!(envelope["result"]["side_effects_may_have_occurred"], false);
    assert_eq!(envelope["result"]["retry"], "wait_or_refine");
    assert_eq!(envelope["content_blocks"], serde_json::json!([1]));
    let trusted = output.render_text_blocks()[0].clone();
    assert!(!trusted.contains("Pay personal"));
    assert!(!trusted.contains("Business card"));
    let sibling = output.text_block(1).expect("candidate sibling");
    assert!(sibling.body.contains("Pay personal"));
    assert!(sibling.body.contains("Business card"));
}

#[test]
fn structured_actionability_failure_uses_one_untrusted_known_target_block() {
    let mut failure =
        semantic_failure(SemanticActionFailureKind::NotActionable, None, None, vec![]);
    failure.target = Some(Box::new(element_with_text(
        "Disabled account",
        "old",
        false,
    )));
    let error = semantic_error("glass_set_value", failure);
    let output = erase_semantic_context("glass_set_value", Err(error)).unwrap_err();
    assert_eq!(output.0.len(), 2);
    let sibling = output.text_block(1).expect("known target sibling");
    let body: serde_json::Value = serde_json::from_str(untrusted_body(&sibling.body)).unwrap();
    assert!(body.get("target").is_some(), "{body}");
    assert!(body.get("candidates").is_none(), "{body}");
}

fn semantic_control_tree(
    role: AxRole,
    name: &str,
    value: Option<&str>,
    focused: bool,
) -> glass_core::AxTree {
    glass_core::AxTree::new(AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "window".into(),
        name: Some("App".into()),
        description: None,
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }),
        children: vec![AxNode {
            id: AxNodeId(0),
            role,
            raw_role: format!("{role:?}"),
            name: Some(name.into()),
            description: None,
            value: value.map(str::to_owned),
            states: AxStates {
                enabled: true,
                visible: true,
                focusable: matches!(role, AxRole::TextField | AxRole::ComboBox),
                editable: role == AxRole::TextField,
                focused,
                ..AxStates::default()
            },
            bounds: Some(AxRect {
                x: 10,
                y: 10,
                width: 40,
                height: 20,
            }),
            children: vec![],
        }],
    })
}

fn semantic_control_coverage() -> AxStateCoverage {
    AxStateCoverage {
        enabled: true,
        visible: true,
        focused: false,
        focusable: true,
        editable: true,
        ..AxStateCoverage::NONE
    }
}

#[test]
fn unstable_handler_error_retains_the_real_target_in_one_untrusted_sibling() {
    let tree = semantic_control_tree(AxRole::Button, "Save", None, false);
    let (mut glass, _, _) =
        started_instrumented_glass_with(tree, semantic_control_coverage(), None);
    let args: ClickElementArgs = serde_json::from_str(
        r#"{"target":{"query":"Save","role":"Button"},"mode":"pointer","timeout_ms":0}"#,
    )
    .unwrap();

    let error = click_element_with(&mut glass, &args, ToolContext::UNBOUNDED).unwrap_err();

    assert_eq!(error.code, "unstable_target");
    assert_eq!(error.bound_dispatch, Some(BoundDispatch::NotDispatched));
    assert_eq!(error.siblings.len(), 1);
    let target: serde_json::Value =
        serde_json::from_str(untrusted_body(&error.siblings[0].render_text().unwrap())).unwrap();
    assert_eq!(target["target"]["id"], 1);
    assert_eq!(target["target"]["name"], "Save");
}

#[test]
fn focus_unconfirmed_combines_focus_and_type_dispatch_evidence_truthfully() {
    let args: TypeArgs = serde_json::from_str(
        r#"{"target":{"query":"Account","role":"TextField"},"focus_mode":"native","text":"secret","timeout_ms":0}"#,
    )
    .unwrap();
    let make = || {
        started_instrumented_glass_with(
            semantic_control_tree(AxRole::TextField, "Account", Some("old"), false),
            semantic_control_coverage(),
            None,
        )
        .0
    };
    let mut contextual_glass = make();
    let error = type_text_with(&mut contextual_glass, &args, ToolContext::UNBOUNDED).unwrap_err();
    assert_eq!(error.code, "focus_unconfirmed");
    assert_eq!(error.bound_dispatch, Some(BoundDispatch::MayHaveDispatched));
    let result = error.result.as_ref().unwrap();
    assert_eq!(result["dispatch"], "not_dispatched");
    assert_eq!(result["side_effects_may_have_occurred"], true);
    assert_eq!(result["focus"]["dispatch"], "dispatched");
    assert_eq!(error.siblings.len(), 1);
    let target: serde_json::Value =
        serde_json::from_str(untrusted_body(&error.siblings[0].render_text().unwrap())).unwrap();
    assert_eq!(target["target"]["id"], 1);
    assert_eq!(target["target"]["name"], serde_json::Value::Null);

    let mut standalone_glass = make();
    let output = type_text(&mut standalone_glass, &args).unwrap_err();
    let envelope = error_envelope(&output);
    assert_eq!(envelope["result"]["dispatch"], "not_dispatched");
    assert_eq!(envelope["result"]["side_effects_may_have_occurred"], true);
    assert_eq!(envelope["content_blocks"], serde_json::json!([1]));
}

#[test]
fn noop_set_value_return_failure_preserves_not_dispatched_provenance() {
    let args: SetValueArgs = serde_json::from_str(
        r#"{"target":{"query":"already","role":"ComboBox"},"text":"already","timeout_ms":0,"return":"settle"}"#,
    )
    .unwrap();
    let make = || {
        started_instrumented_glass_with(
            semantic_control_tree(AxRole::ComboBox, "already", Some("already"), false),
            semantic_control_coverage(),
            None,
        )
        .0
    };
    let mut contextual_glass = make();
    let error = set_value_with(&mut contextual_glass, &args, ToolContext::UNBOUNDED).unwrap_err();
    assert_eq!(error.code, "action_deadline_exceeded");
    assert_eq!(error.bound_dispatch, Some(BoundDispatch::NotDispatched));
    assert!(!error.message.contains("value was written"));
    let result = error.result.as_ref().unwrap();
    assert_eq!(result["dispatch"], "not_dispatched");
    assert_eq!(result["side_effects_may_have_occurred"], false);
    assert_eq!(result["retry"], "do_not_retry");

    let mut standalone_glass = make();
    let output = set_value(&mut standalone_glass, &args).unwrap_err();
    let envelope = error_envelope(&output);
    assert_eq!(envelope["result"]["dispatch"], "not_dispatched");
    assert_eq!(envelope["result"]["side_effects_may_have_occurred"], false);
}

#[test]
fn caller_owned_or_expired_semantic_context_uses_sequence_deadline_and_keeps_evidence() {
    let error = semantic_error(
        "glass_click_element",
        semantic_failure(
            SemanticActionFailureKind::NoMatch,
            Some(Whose::Caller),
            Some(Whose::Caller),
            vec![],
        ),
    );
    assert_eq!(error.code, "sequence_deadline_exceeded");
    assert_eq!(error.category, SafeErrorCategory::SequenceDeadlineExceeded);
    let result = error.result.expect("semantic evidence");
    assert_eq!(result["resolution"]["timed_out_by"], "sequence");
    assert_eq!(result["resolution"]["matches_in_walk"], 1);
    assert_eq!(result["resolution"]["search_complete"], true);

    let action_timeout = semantic_error(
        "glass_click_element",
        semantic_failure(
            SemanticActionFailureKind::NoMatch,
            Some(Whose::Callee),
            Some(Whose::Callee),
            vec![],
        ),
    );
    assert_eq!(action_timeout.code, "no_match");
    assert_eq!(
        action_timeout.result.unwrap()["resolution"]["timed_out_by"],
        "action"
    );
}

#[test]
fn structured_backend_stale_and_deadline_errors_do_not_format_backend_source() {
    let mut stale = semantic_failure(SemanticActionFailureKind::ActionFailed, None, None, vec![]);
    stale.source = Some(GlassError::AxElementChanged(4));
    let stale = semantic_error("glass_click_element", stale);
    assert_eq!(stale.code, "stale_element");
    assert_eq!(stale.category, SafeErrorCategory::StaleElement);

    for (kind, owner, code) in [
        (
            SemanticActionFailureKind::ActionDeadlineExceeded,
            Some(Whose::Callee),
            "action_deadline_exceeded",
        ),
        (
            SemanticActionFailureKind::SequenceDeadlineExceeded,
            Some(Whose::Caller),
            "sequence_deadline_exceeded",
        ),
    ] {
        let rendered = semantic_error(
            "glass_click_element",
            semantic_failure(kind, owner, owner, vec![]),
        );
        assert_eq!(rendered.code, code);
        assert!(!rendered.message.contains("APP CONTROLLED BACKEND DETAIL"));
    }
}

#[test]
fn structured_type_failure_excludes_submitted_payload_from_blocks_and_artifacts() {
    const SENTINEL: &str = "TYPE_PAYLOAD_SENTINEL_7e3e5037";
    let mut failure = semantic_failure(
        SemanticActionFailureKind::FocusUnconfirmed,
        None,
        None,
        vec![semantic_match(SENTINEL, SENTINEL)],
    );
    failure.source = Some(GlassError::Backend(format!("backend echoed {SENTINEL}")));
    let output = erase_semantic_context("glass_type", Err(semantic_error("glass_type", failure)))
        .unwrap_err();
    assert!(
        output
            .render_text_blocks()
            .iter()
            .all(|block| !block.contains(SENTINEL))
    );

    assert_forced_artifacts_exclude_payload(output, "glass_type", SENTINEL);
}

pub(crate) fn targeted_type_snapshot_output_for_server(sentinel: &str) -> crate::tools::ToolOutput {
    let mut tree = semantic_control_tree(AxRole::TextField, "Account", Some(sentinel), true);
    tree.root.children.extend((0..600).map(|index| AxNode {
        id: AxNodeId(0),
        role: AxRole::Label,
        raw_role: "label".into(),
        name: Some(format!("{sentinel}-{index}")),
        description: Some(sentinel.into()),
        value: Some(sentinel.into()),
        states: AxStates {
            visible: true,
            ..AxStates::default()
        },
        bounds: None,
        children: vec![],
    }));
    tree.subject = Some(glass_core::Subject {
        asked: sentinel.into(),
        actual: sentinel.into(),
    });
    tree.assign_ids();
    let coverage = AxStateCoverage {
        focused: true,
        ..semantic_control_coverage()
    };
    let (mut glass, _, _) = started_instrumented_glass_with(tree, coverage, None);
    let args: TypeArgs = serde_json::from_value(serde_json::json!({
        "target": {"query": "Account", "role": "TextField"},
        "focus_mode": "native",
        "text": sentinel,
        "timeout_ms": 1_000,
        "max_nodes": 0,
        "return": "snapshot",
    }))
    .unwrap();
    type_text(&mut glass, &args).unwrap()
}

#[test]
fn targeted_type_snapshot_clears_submitted_and_coincident_text_before_output_policy() {
    const SENTINEL: &str = "TARGETED_TYPE_SNAPSHOT_SENTINEL_ef317";
    let output = targeted_type_snapshot_output_for_server(SENTINEL);
    assert!(output.text_bytes() > crate::output_policy::MAX_TEXT_BYTES);
    assert!(
        output
            .render_text_blocks()
            .iter()
            .all(|block| !block.contains(SENTINEL))
    );
}

fn assert_forced_artifacts_exclude_payload(
    mut output: crate::tools::ToolOutput,
    tool: &'static str,
    sentinel: &str,
) {
    output.0.push(crate::tools::OutContent::trusted_guidance(
        "safe-padding".repeat(900),
    ));
    let root = tempfile::tempdir().unwrap();
    let store = crate::artifacts::ArtifactStore::for_test(root.path(), 1 << 20).unwrap();
    let applied = crate::output_policy::OutputPolicy::new(store.clone()).apply(
        crate::output_policy::ToolCallOutcome {
            tool,
            effect: crate::output::ToolEffect::MayMutate,
            is_error: true,
            target_access: crate::output::TargetAccess::NoActiveTarget,
            output,
        },
    );
    let resources = applied
        .output
        .0
        .iter()
        .filter_map(|content| match content {
            crate::tools::OutContent::ResourceLink(descriptor) => Some(descriptor),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!resources.is_empty(), "forced output must externalize");
    for descriptor in resources {
        let artifact = store.read(descriptor.uri()).unwrap();
        assert!(!artifact.text.contains(sentinel));
    }
}

#[test]
fn untargeted_type_scrubs_payload_from_context_batch_standalone_and_artifacts() {
    const SENTINEL: &str = "UNTARGETED_TYPE_CONTEXT_SENTINEL_91f2";
    let args = TypeArgs {
        target: None,
        focus_mode: None,
        timeout_ms: None,
        max_nodes: None,
        text: SENTINEL.into(),
        return_: None,
    };
    let make = || {
        started_instrumented_glass_with_errors(
            crate::tools::testutil::empty_tree(),
            AxStateCoverage::NONE,
            None,
            Some(format!("backend echoed {SENTINEL}")),
            None,
        )
        .0
    };

    let mut contextual_glass = make();
    let contextual =
        type_text_with(&mut contextual_glass, &args, ToolContext::UNBOUNDED).unwrap_err();
    assert!(!contextual.message.contains(SENTINEL));

    let mut standalone_glass = make();
    let standalone = type_text(&mut standalone_glass, &args).unwrap_err();
    assert!(
        standalone
            .render_text_blocks()
            .iter()
            .all(|b| !b.contains(SENTINEL))
    );
    assert_forced_artifacts_exclude_payload(standalone, "glass_type", SENTINEL);

    let mut batch_glass = make();
    let batch = do_actions(
        &mut batch_glass,
        &DoArgs {
            actions: vec![Action::Type(args)],
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap_err();
    assert!(
        batch
            .render_text_blocks()
            .iter()
            .all(|b| !b.contains(SENTINEL))
    );
}

#[test]
fn legacy_set_value_scrubs_payload_from_context_batch_standalone_and_artifacts() {
    const SENTINEL: &str = "LEGACY_SET_CONTEXT_SENTINEL_6aa4";
    let args = SetValueArgs {
        id: Some(1),
        target: None,
        timeout_ms: None,
        max_nodes: None,
        text: SENTINEL.into(),
        return_: None,
    };
    let make = || {
        let mut glass = started_instrumented_glass_with_errors(
            semantic_control_tree(AxRole::TextField, "Account", Some("old"), false),
            semantic_control_coverage(),
            None,
            None,
            Some(format!("backend echoed {SENTINEL}")),
        )
        .0;
        glass.a11y_snapshot(None).unwrap();
        glass
    };

    let mut contextual_glass = make();
    let contextual =
        set_value_with(&mut contextual_glass, &args, ToolContext::UNBOUNDED).unwrap_err();
    assert_eq!(contextual.code, "transport_failure");
    assert!(!contextual.message.contains(SENTINEL));

    let mut standalone_glass = make();
    let standalone = set_value(&mut standalone_glass, &args).unwrap_err();
    assert!(
        standalone
            .render_text_blocks()
            .iter()
            .all(|b| !b.contains(SENTINEL))
    );
    assert_forced_artifacts_exclude_payload(standalone, "glass_set_value", SENTINEL);

    let mut batch_glass = make();
    let batch = do_actions(
        &mut batch_glass,
        &DoArgs {
            actions: vec![Action::SetValue(args)],
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap_err();
    assert!(
        batch
            .render_text_blocks()
            .iter()
            .all(|b| !b.contains(SENTINEL))
    );
}

fn semantic_success_with_total_text_bytes(total: usize) -> crate::tools::ToolOutput {
    let mut output = success_output(
        "glass_click_element",
        &outcome(
            ActionMethod::Pointer {
                native_fallback: None,
            },
            None,
            true,
            "",
        ),
        None,
        vec![],
    );
    output.0[1] = crate::tools::OutContent::untrusted_observation(
        &serde_json::json!({"target": {"padding": ""}}).to_string(),
    );
    let padding = "x".repeat(total - output.text_bytes());
    output.0[1] = crate::tools::OutContent::untrusted_observation(
        &serde_json::json!({"target": {"padding": padding}}).to_string(),
    );
    assert_eq!(output.text_bytes(), total);
    output
}

fn semantic_error_with_total_text_bytes(total: usize) -> crate::tools::ToolOutput {
    let mut output = erase_semantic_context(
        "glass_click_element",
        Err(semantic_error(
            "glass_click_element",
            semantic_failure(
                SemanticActionFailureKind::AmbiguousTarget,
                None,
                None,
                vec![semantic_match("", "")],
            ),
        )),
    )
    .unwrap_err();
    output.0[1] = crate::tools::OutContent::untrusted_observation(
        &serde_json::json!({"candidates": [{"padding": ""}]}).to_string(),
    );
    let padding = "x".repeat(total - output.text_bytes());
    output.0[1] = crate::tools::OutContent::untrusted_observation(
        &serde_json::json!({"candidates": [{"padding": padding}]}).to_string(),
    );
    assert_eq!(output.text_bytes(), total);
    output
}

fn assert_output_budget_roundtrip(total: usize, is_error: bool, output: crate::tools::ToolOutput) {
    let original = output.render_text_blocks();
    let root = tempfile::tempdir().unwrap();
    let store = crate::artifacts::ArtifactStore::for_test(root.path(), 1 << 20).unwrap();
    let policy = crate::output_policy::OutputPolicy::new(store.clone());
    let applied = policy.apply(crate::output_policy::ToolCallOutcome {
        tool: "glass_click_element",
        effect: crate::output::ToolEffect::MayMutate,
        is_error,
        target_access: crate::output::TargetAccess::NoActiveTarget,
        output,
    });
    assert!(applied.output.text_bytes() <= crate::output_policy::MAX_TEXT_BYTES);
    assert_eq!(applied.is_error, is_error);
    if total <= crate::output_policy::MAX_TEXT_BYTES {
        assert_eq!(applied.output.render_text_blocks(), original);
        assert!(applied.output_metadata().is_none());
    } else {
        let descriptor = applied
            .output
            .0
            .iter()
            .find_map(|content| match content {
                crate::tools::OutContent::ResourceLink(descriptor) => Some(descriptor),
                _ => None,
            })
            .expect("oversized semantic block externalized");
        let artifact = store.read(descriptor.uri()).unwrap();
        let externalized = if artifact.text.starts_with(crate::untrusted::NOTE) {
            artifact.text
        } else {
            let manifest: serde_json::Value = serde_json::from_str(&artifact.text).unwrap();
            manifest["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|block| block["index"] == 1)
                .and_then(|block| block["text"].as_str())
                .expect("manifest preserves externalized semantic block")
                .to_owned()
        };
        assert_eq!(externalized, original[1]);
        assert!(externalized.starts_with(crate::untrusted::NOTE));
    }
}

#[test]
fn output_budget_success_target_8191_8192_8193_roundtrips_exact_untrusted_block() {
    for total in [8_191, 8_192, 8_193] {
        assert_output_budget_roundtrip(total, false, semantic_success_with_total_text_bytes(total));
    }
}

#[test]
fn output_budget_error_candidates_8191_8192_8193_preserve_error_and_roundtrip() {
    for total in [8_191, 8_192, 8_193] {
        assert_output_budget_roundtrip(total, true, semantic_error_with_total_text_bytes(total));
    }
}

#[test]
fn zero_timeout_return_observe_fails_after_dispatch_without_waiting() {
    let mut tree = crate::tools::testutil::empty_tree();
    tree.root.children.push(AxNode {
        id: AxNodeId(0),
        role: AxRole::Button,
        raw_role: "button".into(),
        name: Some("Save".into()),
        description: None,
        value: None,
        states: AxStates {
            enabled: true,
            visible: true,
            ..AxStates::default()
        },
        bounds: Some(AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        }),
        children: vec![],
    });
    tree.assign_ids();
    let coverage = AxStateCoverage {
        enabled: true,
        visible: true,
        ..AxStateCoverage::NONE
    };
    let (mut glass, counters, _) =
        started_instrumented_glass_with(tree, coverage, Some(AxNodeId(1)));
    let args: ClickElementArgs = serde_json::from_str(
        r#"{"target":{"query":"Save","role":"Button"},"mode":"native","timeout_ms":0,"return":"settle"}"#,
    )
    .unwrap();
    counters.clear();
    let started = std::time::Instant::now();
    let error = click_element_with(&mut glass, &args, ToolContext::UNBOUNDED).unwrap_err();
    assert_eq!(error.code, "action_deadline_exceeded");
    assert_eq!(error.category, SafeErrorCategory::ActionDeadlineExceeded);
    assert_eq!(error.bound_dispatch, Some(BoundDispatch::MayHaveDispatched));
    assert_eq!(
        counters
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == "a11y_invoke")
            .count(),
        1,
        "one native dispatch"
    );
    assert_eq!(
        counters.capture.load(Ordering::SeqCst),
        0,
        "no settle capture"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "return path must not sleep for the settle interval"
    );
}

#[test]
fn semantic_validators_assign_stable_request_codes_without_app_text_codes() {
    let invalid_return: ClickElementArgs =
        serde_json::from_str(r#"{"id":1,"return":"APP_TEXT_CODE"}"#).unwrap();
    let error = validate_click_element_args(&invalid_return).unwrap_err();
    assert_eq!(error.code, "invalid_return");
    assert_ne!(error.code, "APP_TEXT_CODE");

    let invalid_target: ClickElementArgs = serde_json::from_str(r#"{}"#).unwrap();
    assert_eq!(
        validate_click_element_args(&invalid_target)
            .unwrap_err()
            .code,
        "invalid_action_target"
    );

    let invalid_sequence = Action::ClickElement(invalid_target);
    assert_eq!(
        validate_action(&invalid_sequence).unwrap_err().code,
        "invalid_sequence"
    );
    assert_eq!(
        ContextualError::validation("legacy".into()).code,
        "invalid_argument"
    );
}
