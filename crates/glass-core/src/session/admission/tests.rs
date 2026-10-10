use crate::session::test_support::*;
use crate::{
    BackgroundInputCapabilities, BackgroundOperation, BackgroundProfile, BackgroundSupport,
    InputMode, Modifier,
};

type Calls = Arc<Mutex<Vec<&'static str>>>;

struct ProbePlatform {
    calls: Calls,
    selected: Option<WindowId>,
    known_profile: bool,
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
    fn background_input_capabilities(&mut self) -> Result<BackgroundInputCapabilities> {
        self.calls.lock().unwrap().push("capabilities");
        let supported = BackgroundOperation {
            status: BackgroundSupport::Supported,
            reasons: Vec::new(),
        };
        Ok(BackgroundInputCapabilities {
            click: supported.clone(),
            scroll: supported.clone(),
            text: supported,
            profile: self.known_profile.then(|| BackgroundProfile {
                id: "test adapter".into(),
                qualification_reference: "unit test only".into(),
            }),
        })
    }
}

fn backend(calls: Calls, selected: Option<WindowId>) -> Backend {
    Backend {
        platform: Box::new(ProbePlatform {
            calls,
            selected,
            known_profile: true,
        }),
        accessibility: Some(Box::new(FakeAccessibility::new(fake_tree_enabled()))),
    }
}

fn background(selected: Option<WindowId>) -> (Glass, tempfile::TempDir, Calls, CtxLog) {
    background_with_profile(selected, true)
}

fn background_with_profile(
    selected: Option<WindowId>,
    known_profile: bool,
) -> (Glass, tempfile::TempDir, Calls, CtxLog) {
    let dir = tempfile::tempdir().unwrap();
    let calls: Calls = Arc::default();
    let reader = FakeAccessibility::new(fake_tree_enabled());
    let reads = reader.ctx_log.clone();
    let mut pending = Some(Backend {
        platform: Box::new(ProbePlatform {
            calls: calls.clone(),
            selected,
            known_profile,
        }),
        accessibility: Some(Box::new(reader)),
    });
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
    glass
        .start(&AppSpec {
            input_mode: InputMode::Background,
            ..spec()
        })
        .unwrap();
    calls.lock().unwrap().clear();
    (glass, dir, calls, reads)
}

fn assert_refusal<T: std::fmt::Debug>(result: Result<T>) {
    let error = result.unwrap_err();
    assert!(
        matches!(
            error.cause(),
            GlassError::UnsupportedInputMode {
                input_mode: InputMode::Background,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        error.bound_dispatch(),
        Some(crate::BoundDispatch::NotDispatched)
    );
    assert_eq!(error.bound(), None);
}

#[test]
fn background_start_preserves_session_snapshot_and_epoch_before_factory_or_teardown() {
    let dir = tempfile::tempdir().unwrap();
    let calls: Calls = Arc::default();
    let factory_calls = calls.clone();
    let mut glass = Glass::new(
        Box::new(move |_| {
            factory_calls.lock().unwrap().push("factory");
            Ok(backend(factory_calls.clone(), Some(WindowId(1))))
        }),
        "fake".into(),
        BaselineStore::new(dir.path()),
        10,
    );
    glass.start(&spec()).unwrap();
    let tree = glass.a11y_snapshot(None).unwrap();
    let epoch = glass.observation_epoch();
    let generation = epoch.generation();
    calls.lock().unwrap().clear();
    let requested = AppSpec {
        input_mode: InputMode::Background,
        build: Some("must never run".into()),
        ..spec()
    };
    assert_refusal(glass.start(&requested));
    assert_refusal(glass.start_on("another", &requested));
    assert_eq!(glass.active_backend(), Some("fake"));
    assert_eq!(glass.active_input_mode(), Some(InputMode::Foreground));
    assert_eq!(glass.geometry().unwrap(), geometry());
    assert_eq!(epoch.generation(), generation);
    assert_eq!(
        glass.require_active().unwrap().last_ax.as_ref().unwrap(),
        &tree
    );
    assert!(calls.lock().unwrap().is_empty());
    glass.click_element(AxNodeId(1)).unwrap();
    let after = calls.lock().unwrap();
    assert_eq!(after.iter().filter(|&&call| call == "pointer").count(), 1);
    assert!(
        after
            .iter()
            .all(|&call| matches!(call, "geometry" | "pointer"))
    );
}

#[test]
fn background_pointer_shapes_are_checked_before_backend_input() {
    let (mut glass, _dir, calls, _) = background(Some(WindowId(1)));
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
    assert_eq!(&*calls.lock().unwrap(), &["pointer", "pointer"]);
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
fn background_mutations_refuse_before_reads_focus_and_writes() {
    let (mut glass, _dir, calls, reads) = background(Some(WindowId(1)));
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
fn background_semantic_modes_refuse_before_resolution_even_for_empty_text() {
    let (mut glass, _dir, calls, reads) = background(Some(WindowId(1)));
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
            assert_eq!(error.kind, SemanticActionFailureKind::UnsupportedInputMode);
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
            assert_eq!(error.kind, SemanticActionFailureKind::UnsupportedInputMode);
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
        assert_eq!(error.kind, SemanticActionFailureKind::UnsupportedInputMode);
        assert!(error.resolution.is_none());
        assert!(error.focus.is_none());
    }
    assert!(calls.lock().unwrap().is_empty());
    assert!(reads.lock().unwrap().is_none());
}

#[test]
fn background_selection_only_refreshes_the_same_reliably_identified_window() {
    for selected in [Some(WindowId(1)), None] {
        let (mut glass, _dir, calls, _) = background(selected);
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
fn session_capabilities_are_read_only_and_never_authorize_background_text() {
    let (mut glass, _dir, calls, reads) = background(Some(WindowId(1)));
    let epoch = glass.observation_epoch();
    let generation = epoch.generation();
    let report = glass.session_capabilities().unwrap();
    assert_eq!(report.input_mode, InputMode::Background);
    assert_eq!(
        report.background_input.click.status,
        BackgroundSupport::Supported
    );
    assert_eq!(
        report.background_input.text.status,
        BackgroundSupport::Unsupported
    );
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
fn session_capabilities_without_a_qualification_profile_cannot_claim_support() {
    let (mut glass, _dir, _calls, _) = background_with_profile(Some(WindowId(1)), false);
    let report = glass.session_capabilities().unwrap();
    assert!(report.background_input.profile.is_none());
    assert_eq!(
        report.background_input.click.status,
        BackgroundSupport::Unsupported
    );
    assert_eq!(
        report.background_input.scroll.status,
        BackgroundSupport::Unsupported
    );
}
