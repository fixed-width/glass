use crate::session::test_support::*;
use crate::{
    CapabilityStatus, DesktopInterference, InputCapabilities, InputRoute, Modifier, Support,
};

type Calls = Arc<Mutex<Vec<&'static str>>>;

struct ProbePlatform {
    calls: Calls,
    selected: Option<WindowId>,
    classified: bool,
    route: InputRoute,
}

fn geometry() -> WindowGeometry {
    WindowGeometry {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    }
}

impl Platform for ProbePlatform {
    fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
        self.calls.lock().unwrap().push("start");
        Ok(geometry())
    }
    fn stop_app_by(&mut self, _deadline: Deadline) -> Result<()> {
        self.calls.lock().unwrap().push("stop");
        Ok(())
    }
    fn capture_frame_by(&mut self, _region: Option<&Region>, _deadline: Deadline) -> Result<Frame> {
        self.calls.lock().unwrap().push("capture");
        Frame::new(100, 100, vec![255; 40_000])
    }
    fn capture_window_by(
        &mut self,
        _id: WindowId,
        region: Option<&Region>,
        deadline: Deadline,
    ) -> Result<Frame> {
        self.capture_frame_by(region, deadline)
    }
    fn send_pointer_by(&mut self, _event: &PointerEvent, _deadline: Deadline) -> Result<()> {
        self.calls.lock().unwrap().push("pointer");
        Ok(())
    }
    fn send_key_by(&mut self, _event: &KeyEvent, _deadline: Deadline) -> Result<()> {
        self.calls.lock().unwrap().push("key");
        Ok(())
    }
    fn set_clipboard(&mut self, _text: &str) -> Result<()> {
        self.calls.lock().unwrap().push("clipboard");
        Ok(())
    }
    fn window_by(&mut self, op: &WindowOp, _deadline: Deadline) -> Result<WindowGeometry> {
        self.calls
            .lock()
            .unwrap()
            .push(if matches!(op, WindowOp::Geometry) {
                "geometry"
            } else {
                "window mutation"
            });
        Ok(geometry())
    }
    fn list_windows_by(&mut self, _deadline: Deadline) -> Result<Vec<WindowInfo>> {
        Ok(vec![window_info(1, geometry(), true)])
    }
    fn select_window_by(&mut self, id: WindowId, _deadline: Deadline) -> Result<WindowGeometry> {
        self.calls.lock().unwrap().push("selection");
        self.selected = Some(id);
        Ok(geometry())
    }
    fn observation_window_id(&self) -> Option<WindowId> {
        self.selected
    }
    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        Vec::new()
    }
    fn input_route(&self) -> InputRoute {
        self.route
    }
    fn input_capabilities(&self) -> InputCapabilities {
        self.calls.lock().unwrap().push("capabilities");
        if self.classified {
            InputCapabilities::uniform(
                CapabilityStatus::supported(),
                if self.route == InputRoute::SharedDesktop {
                    DesktopInterference::Possible
                } else {
                    DesktopInterference::None
                },
            )
        } else {
            InputCapabilities::unknown()
        }
    }
}

fn backend(calls: Calls, selected: Option<WindowId>) -> Backend {
    Backend {
        platform: Box::new(ProbePlatform {
            calls,
            selected,
            classified: true,
            route: InputRoute::SharedDesktop,
        }),
        accessibility: Some(Box::new(FakeAccessibility::new(fake_tree_enabled()))),
    }
}

fn targeted(selected: Option<WindowId>) -> (Glass, tempfile::TempDir, Calls, CtxLog) {
    targeted_with_support(selected, true)
}

fn targeted_with_support(
    selected: Option<WindowId>,
    classified: bool,
) -> (Glass, tempfile::TempDir, Calls, CtxLog) {
    let dir = tempfile::tempdir().unwrap();
    let calls: Calls = Arc::default();
    let reader = FakeAccessibility::new(fake_tree_enabled());
    let reads = reader.ctx_log.clone();
    let mut pending = Some(Backend {
        platform: Box::new(ProbePlatform {
            calls: calls.clone(),
            selected,
            classified,
            route: InputRoute::WindowTargeted,
        }),
        accessibility: Some(Box::new(reader)),
    });
    let mut glass = Glass::new(
        Box::new(move |_| Ok(pending.take().unwrap())),
        "fake".into(),
        BaselineStore::new(dir.path()),
        10,
    );
    glass.start(&spec()).unwrap();
    calls.lock().unwrap().clear();
    (glass, dir, calls, reads)
}

fn assert_refusal<T: std::fmt::Debug>(result: Result<T>) {
    let error = result.unwrap_err();
    assert!(
        matches!(error.cause(), GlassError::UnsupportedOperation { .. }),
        "{error:?}"
    );
    assert_eq!(
        error.bound_dispatch(),
        Some(crate::BoundDispatch::NotDispatched)
    );
    assert_eq!(error.bound(), None);
}

#[test]
fn ordinary_sessions_keep_typing_and_pointer_operations_without_a_mode() {
    for route in [InputRoute::Isolated, InputRoute::SharedDesktop] {
        let dir = tempfile::tempdir().unwrap();
        let calls: Calls = Arc::default();
        let mut built = backend(calls.clone(), Some(WindowId(1)));
        built.platform = Box::new(ProbePlatform {
            calls: calls.clone(),
            selected: Some(WindowId(1)),
            classified: true,
            route,
        });
        let mut pending = Some(built);
        let mut glass = Glass::new(
            Box::new(move |_| Ok(pending.take().unwrap())),
            "fake".into(),
            BaselineStore::new(dir.path()),
            10,
        );
        glass.start(&spec()).unwrap();
        calls.lock().unwrap().clear();
        glass.key(&KeyEvent::Text("normal typing".into())).unwrap();
        glass.pointer(&PointerEvent::Move { x: 1, y: 1 }).unwrap();
        glass
            .pointer(&PointerEvent::Click {
                x: 1,
                y: 1,
                button: MouseButton::Right,
                count: 2,
                modifiers: vec![Modifier::Shift],
            })
            .unwrap();
        assert_eq!(&*calls.lock().unwrap(), &["key", "pointer", "pointer"]);
    }
}

#[test]
fn window_targeted_pointer_shapes_are_checked_before_backend_input() {
    let (mut glass, _dir, calls, _) = targeted(Some(WindowId(1)));
    let click = |button, count, modifiers| PointerEvent::Click {
        x: 20,
        y: 20,
        button,
        count,
        modifiers,
    };
    let scroll = |dx, modifiers| PointerEvent::Scroll {
        x: 20,
        y: 20,
        dx,
        dy: 1,
        modifiers,
    };
    glass.pointer(&click(MouseButton::Left, 1, vec![])).unwrap();
    glass.pointer(&scroll(0, vec![])).unwrap();
    assert_eq!(
        &*calls.lock().unwrap(),
        &["capabilities", "pointer", "capabilities", "pointer"]
    );
    calls.lock().unwrap().clear();
    for event in [
        click(MouseButton::Right, 1, vec![]),
        click(MouseButton::Middle, 1, vec![]),
        click(MouseButton::Left, 2, vec![]),
        click(MouseButton::Left, 1, vec![Modifier::Shift]),
        scroll(1, vec![]),
        scroll(0, vec![Modifier::Control]),
        PointerEvent::Move { x: 20, y: 20 },
        PointerEvent::Drag {
            from_x: 1,
            from_y: 1,
            to_x: 2,
            to_y: 2,
            button: MouseButton::Left,
            modifiers: vec![],
            duration_ms: 100,
        },
        PointerEvent::Gesture {
            pointers: vec![],
            duration_ms: 100,
        },
    ] {
        assert_refusal(glass.pointer(&event));
    }
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn window_targeted_mutations_refuse_before_reads_focus_and_writes() {
    let (mut glass, _dir, calls, reads) = targeted(Some(WindowId(1)));
    for event in [
        KeyEvent::Chord("enter".into()),
        KeyEvent::Text("".into()),
        KeyEvent::Text("secret".into()),
    ] {
        assert_refusal(glass.key(&event));
    }
    assert_refusal(glass.set_clipboard(""));
    for op in [
        WindowOp::Focus,
        WindowOp::Move { x: 1, y: 1 },
        WindowOp::Resize {
            width: 10,
            height: 10,
        },
    ] {
        assert_refusal(glass.window(&op));
    }
    assert_refusal(glass.click_element(AxNodeId(1)));
    assert_refusal(glass.set_value(AxNodeId(1), ""));
    assert_refusal(glass.scroll_to_element(&ScrollToElementParams {
        name: Some("Save".into()),
        description: None,
        role: None,
        value_contains: None,
        direction: None,
        anchor: None,
        step: 1,
        timeout_ms: 100,
    }));
    assert!(calls.lock().unwrap().is_empty());
    assert!(reads.lock().unwrap().is_none());
}

#[test]
fn window_targeted_semantic_modes_refuse_before_resolution_even_for_empty_text() {
    let (mut glass, _dir, calls, reads) = targeted(Some(WindowId(1)));
    let semantic = SemanticTarget {
        target: crate::SemanticSelector::new(Some("Save".into()), None, Vec::new()).unwrap(),
        within: None,
    };
    for mode in [ActionMode::Auto, ActionMode::Native, ActionMode::Pointer] {
        for target in [
            ActionTarget::Id(AxNodeId(1)),
            ActionTarget::Semantic(semantic.clone()),
        ] {
            let error = glass
                .click_target(&ClickTargetParams {
                    target: target.clone(),
                    mode,
                    timeout_ms: None,
                    max_nodes: None,
                })
                .unwrap_err();
            assert_eq!(error.kind, SemanticActionFailureKind::UnsupportedOperation);
            assert!(error.resolution.is_none());
            assert!(!error.side_effects_may_have_occurred());
            let error = glass
                .set_value_target(
                    &SetValueTargetParams {
                        target,
                        timeout_ms: None,
                        max_nodes: None,
                    },
                    "",
                )
                .unwrap_err();
            assert_eq!(error.kind, SemanticActionFailureKind::UnsupportedOperation);
            assert!(error.resolution.is_none());
        }
        let error = glass
            .type_target(
                &TypeTargetParams {
                    target: semantic.clone(),
                    focus_mode: mode,
                    timeout_ms: 100,
                    max_nodes: None,
                },
                "",
            )
            .unwrap_err();
        assert_eq!(error.kind, SemanticActionFailureKind::UnsupportedOperation);
        assert!(error.resolution.is_none());
        assert!(error.focus.is_none());
    }
    assert!(calls.lock().unwrap().is_empty());
    assert!(reads.lock().unwrap().is_none());
}

#[test]
fn window_targeted_selection_only_refreshes_the_same_reliably_identified_window() {
    for selected in [Some(WindowId(1)), None] {
        let (mut glass, _dir, calls, _) = targeted(selected);
        let epoch = glass.observation_epoch();
        let generation = epoch.generation();
        assert_refusal(glass.select_window(WindowId(2)));
        if selected.is_some() {
            assert_eq!(glass.select_window(WindowId(1)).unwrap(), geometry());
            assert_eq!(&*calls.lock().unwrap(), &["geometry"]);
        } else {
            assert_refusal(glass.select_window(WindowId(1)));
            assert!(calls.lock().unwrap().is_empty());
        }
        assert_eq!(epoch.generation(), generation);
    }
}

#[test]
fn session_capabilities_are_read_only_and_limit_only_window_targeted_text() {
    let (mut glass, _dir, calls, reads) = targeted(Some(WindowId(1)));
    let epoch = glass.observation_epoch();
    let generation = epoch.generation();
    let report = glass.session_capabilities().unwrap();
    assert_eq!(report.input.click.support.status, Support::Supported);
    assert_eq!(
        report.input.click.desktop_interference,
        DesktopInterference::None
    );
    assert_eq!(report.input.text.support.status, Support::Unsupported);
    assert!(report.input.click.restriction.unwrap().contains("semantic"));
    assert_eq!(&*calls.lock().unwrap(), &["capabilities"]);
    assert_eq!(epoch.generation(), generation);
    assert!(reads.lock().unwrap().is_none());
    glass.stop().unwrap();
    assert!(matches!(
        glass.session_capabilities(),
        Err(GlassError::NoActiveSession)
    ));
}

#[test]
fn an_unclassified_platform_does_not_claim_input_support_or_isolation() {
    let calls: Calls = Arc::default();
    let platform = FakePlatform::new(100, 100).with_event_log(calls.clone());
    assert_eq!(platform.input_route(), InputRoute::SharedDesktop);
    let report = platform.input_capabilities();
    for operation in [report.click, report.scroll, report.text] {
        assert_eq!(operation.support.status, Support::Unsupported);
        assert_eq!(operation.desktop_interference, DesktopInterference::Unknown);
        assert!(operation.support.note.is_some());
        assert!(operation.restriction.is_none());
    }
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn unqualified_window_targeted_support_remains_unavailable() {
    let (glass, _dir, _calls, _) = targeted_with_support(Some(WindowId(1)), false);
    let report = glass.session_capabilities().unwrap();
    assert_eq!(report.input.click.support.status, Support::Unsupported);
    assert_eq!(report.input.scroll.support.status, Support::Unsupported);
}

#[test]
fn targeted_shape_still_requires_current_non_interfering_support() {
    let (mut glass, _dir, calls, _) = targeted_with_support(Some(WindowId(1)), false);
    assert_refusal(glass.pointer(&PointerEvent::Click {
        x: 1,
        y: 1,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    }));
    assert_refusal(glass.pointer(&PointerEvent::Scroll {
        x: 1,
        y: 1,
        dx: 0,
        dy: 1,
        modifiers: vec![],
    }));
    assert_eq!(&*calls.lock().unwrap(), &["capabilities", "capabilities"]);
}

#[test]
fn targeted_input_requires_both_supported_status_and_no_desktop_interference() {
    for interference in [DesktopInterference::Possible, DesktopInterference::Unknown] {
        let capability =
            crate::InputOperationCapability::new(CapabilityStatus::supported(), interference);
        assert_refusal(super::require_targeted_support(capability, "click"));
    }
    for support in [
        CapabilityStatus::degraded("partial"),
        CapabilityStatus::requires_setup("setup"),
        CapabilityStatus::unsupported(Some("unsupported")),
    ] {
        let capability = crate::InputOperationCapability::new(support, DesktopInterference::None);
        assert_refusal(super::require_targeted_support(capability, "scroll"));
    }
}

#[test]
fn targeted_capability_reporting_agrees_with_admission_for_partial_or_interfering_input() {
    for support in [
        CapabilityStatus::supported(),
        CapabilityStatus::degraded("partial"),
        CapabilityStatus::requires_setup("setup"),
        CapabilityStatus::unsupported(Some("unsupported")),
    ] {
        for interference in [
            DesktopInterference::None,
            DesktopInterference::Possible,
            DesktopInterference::Unknown,
        ] {
            let mut report = InputCapabilities::uniform(support, interference);
            report.restrict_window_targeted();
            let admitted = super::require_targeted_support(report.click.clone(), "click").is_ok();
            assert_eq!(
                admitted,
                support.status == Support::Supported && interference == DesktopInterference::None
            );
            if support.status == Support::RequiresSetup {
                assert_eq!(report.click.support.status, Support::RequiresSetup);
                assert_eq!(report.click.support.note, Some("setup"));
            } else if !admitted {
                assert_eq!(report.click.support.status, Support::Unsupported);
            }
            assert_eq!(report.text.support.status, Support::Unsupported);
        }
    }
}
