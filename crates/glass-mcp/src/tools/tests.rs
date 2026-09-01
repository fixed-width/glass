use super::testutil::*;
use super::*;
use glass_core::{AppSpec, Frame, SandboxLevel};

fn start_args() -> StartArgs {
    StartArgs {
        build: None,
        run: vec!["app".into()],
        backend: None,
        sandbox: None,
        cwd: None,
        env: std::collections::BTreeMap::new(),
        window_hint: None,
        timeout_ms: None,
        a11y: None,
    }
}

#[test]
fn a11y_defaults_on_when_omitted() {
    // The a11y-first path is the low-token default, so an omitted flag enables it.
    assert!(resolve_a11y(None), "omitted a11y must default on");
    assert!(resolve_a11y(Some(true)));
    assert!(!resolve_a11y(Some(false)), "explicit false opts out");
}

#[test]
fn floor_unset_preserves_current_behavior() {
    // arg wins over env; omit → GLASS_SANDBOX else default. Floor off = no enforcement.
    assert_eq!(
        resolve_sandbox(Some("off"), Some("strict"), None).unwrap(),
        SandboxLevel::Off
    );
    assert_eq!(
        resolve_sandbox(None, Some("strict"), None).unwrap(),
        SandboxLevel::Strict
    );
    assert_eq!(
        resolve_sandbox(None, None, None).unwrap(),
        SandboxLevel::Default
    );
}

#[test]
fn floor_clamps_an_omitted_request_up() {
    // omit-default default, floor strict → effective strict (policy applies, no error).
    assert_eq!(
        resolve_sandbox(None, None, Some("strict")).unwrap(),
        SandboxLevel::Strict
    );
    assert_eq!(
        resolve_sandbox(None, Some("off"), Some("default")).unwrap(),
        SandboxLevel::Default
    );
}

#[test]
fn floor_honors_an_explicit_request_at_or_above_it() {
    assert_eq!(
        resolve_sandbox(Some("strict"), None, Some("default")).unwrap(),
        SandboxLevel::Strict
    );
    assert_eq!(
        resolve_sandbox(Some("default"), None, Some("default")).unwrap(),
        SandboxLevel::Default
    );
}

#[test]
fn floor_refuses_an_explicit_request_below_it() {
    let err = resolve_sandbox(Some("off"), None, Some("strict")).unwrap_err();
    assert!(err.contains("GLASS_SANDBOX_FLOOR=strict"), "{err}");
    assert!(err.contains("off"), "{err}");
    assert!(resolve_sandbox(Some("default"), None, Some("strict")).is_err());
}

#[test]
fn invalid_floor_or_level_is_an_error() {
    assert!(resolve_sandbox(None, None, Some("bogus")).is_err());
    assert!(resolve_sandbox(Some("bogus"), None, None).is_err());
}

#[test]
fn floor_from_var_maps_present_absent_and_non_utf8() {
    // Present + valid → Some; absent → None (no floor).
    assert_eq!(
        floor_from_var(Ok("strict".to_string())).unwrap(),
        Some("strict".to_string())
    );
    assert_eq!(
        floor_from_var(Err(std::env::VarError::NotPresent)).unwrap(),
        None
    );
    // Set-but-non-UTF-8 must be an ERROR (fail-closed), never silently unset (fail-open) —
    // otherwise a garbled operator floor would silently disable the policy.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let bad = std::ffi::OsString::from_vec(vec![0x73, 0x80, 0x74]); // invalid UTF-8
        assert!(floor_from_var(Err(std::env::VarError::NotUnicode(bad))).is_err());
    }
}

#[test]
fn start_returns_geometry_json() {
    let mut g = glass_with(FakePlatform::new(80, 60));
    let out = start(&mut g, &start_args()).unwrap();
    let v = assert_envelope(&out, "glass_start");
    assert_eq!(v["width"], json!(80));
    assert_eq!(v["height"], json!(60));
}

/// The link a wayland smoke run rests on: `glass_start`'s `env` argument
/// reaching the backend as `AppSpec.env`. Every smoke check passes with that env removed, so
/// no run observes it.
#[test]
fn start_hands_the_requested_env_to_the_backend() {
    use std::sync::{Arc, Mutex};
    let specs: Arc<Mutex<Vec<AppSpec>>> = Arc::new(Mutex::new(Vec::new()));
    let mut g = glass_with(FakePlatform::new(10, 10).with_spec_log(specs.clone()));
    let mut a = start_args();
    a.env.insert("GDK_BACKEND".into(), "wayland".into());
    start(&mut g, &a).unwrap();
    assert_eq!(
        specs.lock().unwrap()[0].env,
        vec![("GDK_BACKEND".to_string(), "wayland".to_string())]
    );
}

#[test]
fn start_rejects_empty_run() {
    let mut g = glass_with(FakePlatform::new(10, 10));
    let mut a = start_args();
    a.run.clear();
    assert!(start(&mut g, &a).is_err());
}

#[test]
fn start_rejects_unknown_sandbox() {
    // Locks rejection at the `glass_start` tool boundary (not just the
    // `resolve_sandbox`/`SandboxLevel::FromStr` units below it) — an unknown
    // `sandbox` must not be silently coerced to the default level.
    let mut g = glass_with(FakePlatform::new(10, 10));
    let mut a = start_args();
    a.sandbox = Some("bogus".into());
    let err = start(&mut g, &a).unwrap_err();
    assert!(err.contains("unknown sandbox level"), "got: {err}");
}

#[test]
fn stop_without_session_errors_with_message() {
    let mut g = glass_with(FakePlatform::new(10, 10));
    let err = stop(&mut g).unwrap_err();
    assert!(err.contains("no active session"));
}

#[test]
fn stop_running_session_returns_empty_envelope() {
    let mut g = glass_with(FakePlatform::new(10, 10));
    start(&mut g, &start_args()).unwrap();
    let out = stop(&mut g).unwrap();
    let v = assert_envelope(&out, "glass_stop");
    assert_eq!(v, json!({}), "envelope: {v}");
}

#[test]
fn window_resize_requires_dimensions() {
    let mut g = glass_with(FakePlatform::new(10, 10));
    start(&mut g, &start_args()).unwrap();
    let a = WindowArgs {
        op: "resize".into(),
        x: None,
        y: None,
        width: None,
        height: None,
    };
    assert!(window(&mut g, &a).unwrap_err().contains("width"));
}

#[test]
fn window_resize_updates_and_returns_geometry() {
    let mut g = glass_with(FakePlatform::new(10, 10));
    start(&mut g, &start_args()).unwrap();
    let a = WindowArgs {
        op: "resize".into(),
        x: None,
        y: None,
        width: Some(33),
        height: Some(44),
    };
    let out = window(&mut g, &a).unwrap();
    let v = assert_envelope(&out, "glass_window");
    assert_eq!(v["width"], json!(33));
    assert_eq!(v["height"], json!(44));
}

#[test]
fn window_rejects_unknown_op() {
    let mut g = glass_with(FakePlatform::new(10, 10));
    start(&mut g, &start_args()).unwrap();
    let a = WindowArgs {
        op: "levitate".into(),
        x: None,
        y: None,
        width: None,
        height: None,
    };
    let err = window(&mut g, &a).unwrap_err();
    assert!(err.contains("unknown window op"), "got: {err}");
}

#[test]
fn parse_button_maps_and_rejects() {
    assert!(matches!(
        parse_button(Some("middle")),
        Ok(MouseButton::Middle)
    ));
    assert!(matches!(parse_button(None), Ok(MouseButton::Left)));
    assert!(parse_button(Some("nope")).is_err());
}

#[test]
fn a11y_snapshot_returns_outline_text() {
    let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let out = a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    assert_envelope(&out, "glass_a11y_snapshot");
    match &out.0[1] {
        OutContent::Text(t) => {
            assert!(
                t.starts_with(crate::untrusted::NOTE),
                "must be marked untrusted: {t}"
            );
            assert!(
                t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"),
                "enveloped: {t}"
            );
            assert!(t.contains("#0 Window"), "outline: {t}");
            assert!(
                t.contains("#1 Button \"Save\" (10,10 20x20)"),
                "outline: {t}"
            );
        }
        _ => panic!("expected text"),
    }
}

#[test]
fn a11y_snapshot_appends_pixel_hint_when_treeless() {
    let mut g = glass_with_a11y(FakePlatform::new(100, 100), empty_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let out = a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    assert_envelope(&out, "glass_a11y_snapshot");
    // [0]=envelope, [1]=untrusted root-only outline, [2]=glass's trusted pixel hint.
    match &out.0[2] {
        OutContent::Text(t) => {
            assert!(t.contains("glass_screenshot"), "pixel hint: {t}");
            assert!(
                !t.starts_with(crate::untrusted::NOTE),
                "the hint is glass's own guidance, not untrusted app content: {t}"
            );
        }
        _ => panic!("expected the pixel-hint text"),
    }
}

#[test]
fn a11y_snapshot_truncation_steer_is_a_trusted_block_outside_the_untrusted_envelope() {
    // glass's own truncation steer must not be baked into `render_compact`'s output, or
    // it ends up inside the untrusted envelope — under a directive telling the agent to
    // ignore instructions in that block, even though the steer ("drive by pixels…") IS
    // one of glass's own. It gets its own trusted, unwrapped block.
    let mut g = glass_with_a11y(FakePlatform::new(100, 100), truncated_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let out = a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    assert_envelope(&out, "glass_a11y_snapshot");
    // [0]=envelope, [1]=untrusted-wrapped outline, [2]=glass's trusted truncation steer.
    assert_eq!(
        out.0.len(),
        3,
        "envelope + wrapped outline + trusted truncation steer"
    );
    match &out.0[1] {
        OutContent::Text(t) => {
            assert!(
                t.starts_with(crate::untrusted::NOTE)
                    && t.contains("⟦untrusted:")
                    && t.contains("⟦/untrusted:"),
                "the outline itself stays untrusted-wrapped: {t}"
            );
            assert!(
                !t.contains("truncated"),
                "the truncation notice must NOT be baked into the untrusted-wrapped \
                 outline body: {t}"
            );
        }
        _ => panic!("expected the wrapped outline text"),
    }
    match &out.0[2] {
        OutContent::Text(t) => {
            assert!(t.contains("truncated"), "truncation steer: {t}");
            assert!(
                t.contains("glass_screenshot"),
                "the steer names the pixel fallback: {t}"
            );
            assert!(
                !t.starts_with(crate::untrusted::NOTE) && !t.contains("⟦untrusted:"),
                "the steer is glass's own trusted guidance, outside the untrusted \
                 markers entirely: {t}"
            );
        }
        _ => panic!("expected the trusted truncation-steer text"),
    }
}

#[test]
fn a11y_snapshot_discloses_an_unpublished_document_as_a_trusted_block() {
    let mut g = glass_with_a11y(FakePlatform::new(100, 100), unpublished_document_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let out = a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    assert_envelope(&out, "glass_a11y_snapshot");
    assert_eq!(
        out.0.len(),
        3,
        "envelope + wrapped outline + document guidance"
    );
    match (&out.0[1], &out.0[2]) {
        (OutContent::Text(body), OutContent::Text(steer)) => {
            assert!(
                !body.contains("has no readable content"),
                "guidance must not be inside the untrusted body: {body}"
            );
            assert!(steer.contains("Document"), "{steer}");
            assert!(steer.contains("glass_screenshot"), "{steer}");
        }
        other => panic!("unexpected blocks: {other:?}"),
    }
}

#[test]
fn a11y_snapshot_discloses_withheld_content_as_a_trusted_block() {
    let mut tree = fake_tree();
    tree.unexposed = 1;
    let mut g = glass_with_a11y(FakePlatform::new(100, 100), tree);
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let out = a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    assert_envelope(&out, "glass_a11y_snapshot");
    assert_eq!(
        out.0.len(),
        3,
        "envelope + wrapped outline + the withheld-content steer"
    );
    match (&out.0[1], &out.0[2]) {
        (OutContent::Text(body), OutContent::Text(steer)) => {
            assert!(
                !body.contains("placeholder"),
                "the steer must not be inside the untrusted body: {body}"
            );
            assert!(steer.contains("has not exposed"), "{steer}");
            assert!(steer.contains("glass_screenshot"), "{steer}");
            assert!(
                !steer.starts_with(crate::untrusted::NOTE) && !steer.contains("⟦untrusted:"),
                "glass's own guidance stays outside the untrusted markers: {steer}"
            );
        }
        other => panic!("unexpected blocks: {other:?}"),
    }
}

#[test]
fn the_withheld_content_steer_sits_between_the_unreadable_one_and_the_document_one() {
    // All three can fire on one tree.
    let mut tree = unpublished_document_tree();
    tree.unreadable = 1;
    tree.unexposed = 1;
    let steers = a11y_steers(&tree);
    let at = |needle: &str| {
        steers
            .iter()
            .position(|s| s.contains(needle))
            .unwrap_or_else(|| panic!("no steer contains {needle:?}: {steers:?}"))
    };
    assert!(
        at("could not be read") < at("has not exposed"),
        "{steers:?}"
    );
    assert!(
        at("has not exposed") < at("no readable content"),
        "{steers:?}"
    );
}

#[test]
fn a_snapshot_of_another_app_says_so_in_its_text() {
    let mut tree = empty_tree();
    tree.subject = Some(glass_core::Subject {
        asked: "com.example.app".into(),
        actual: "com.google.android.permissioncontroller".into(),
    });
    let steers = a11y_steers(&tree);
    assert!(
        steers
            .iter()
            .any(|s| s.contains("com.google.android.permissioncontroller")),
        "the agent is told which app the ids address: {steers:?}"
    );
}

#[test]
fn a11y_snapshot_unsupported_message() {
    let mut g = glass_with(FakePlatform::new(40, 30));
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let err = a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap_err();
    assert!(err.contains("not supported"), "msg: {err}");
}

#[test]
fn set_value_tool_ok_and_errors() {
    let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    let out = set_value(
        &mut g,
        &SetValueArgs {
            id: 1,
            text: "hello".into(),
            return_: None,
        },
    )
    .unwrap();
    let v = assert_envelope(&out, "glass_set_value");
    assert_eq!(v["id"], json!(1), "envelope: {v}");
    // unknown id surfaces the actionable message
    let err = set_value(
        &mut g,
        &SetValueArgs {
            id: 99,
            text: "x".into(),
            return_: None,
        },
    )
    .unwrap_err();
    assert!(err.contains("not in the current snapshot"), "msg: {err}");
}

#[test]
fn set_value_tool_rejects_uneditable_and_stale() {
    let spec = AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    };
    // Backend says the element isn't editable: the tool must surface an error,
    // never the "set value" confirmation (a silent successful-looking no-op is
    // the worst failure for an agent that then asserts "value set").
    let mut g = glass_with_a11y_outcome(
        FakePlatform::new(100, 100),
        fake_tree(),
        SetOutcome::NotEditable,
    );
    g.start(&spec).unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    let err = set_value(
        &mut g,
        &SetValueArgs {
            id: 1,
            text: "x".into(),
            return_: None,
        },
    )
    .unwrap_err();
    assert!(err.contains("not editable"), "msg: {err}");

    // Element changed since the snapshot: same contract — error, not success.
    let mut g = glass_with_a11y_outcome(
        FakePlatform::new(100, 100),
        fake_tree(),
        SetOutcome::Changed,
    );
    g.start(&spec).unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    let err = set_value(
        &mut g,
        &SetValueArgs {
            id: 1,
            text: "x".into(),
            return_: None,
        },
    )
    .unwrap_err();
    assert!(err.contains("changed since the snapshot"), "msg: {err}");
}

#[test]
fn click_element_tool_ok_and_errors() {
    let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    assert!(
        click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: None
            }
        )
        .is_ok()
    );
    let err = click_element(
        &mut g,
        &ClickElementArgs {
            id: 99,
            return_: None,
        },
    )
    .unwrap_err();
    assert!(err.contains("not in the current snapshot"), "msg: {err}");
}

#[test]
fn a11y_marks_returns_image_and_legend() {
    use glass_core::Frame;
    let platform =
        FakePlatform::new(100, 100).with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let mut g = glass_with_a11y(platform, fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let out = a11y_marks(&mut g).unwrap();
    assert!(
        matches!(out.0[0], OutContent::Image(_)),
        "first item is the image"
    );
    let OutContent::Envelope(envelope) = &out.0[1] else {
        panic!("expected envelope as the second item")
    };
    assert_eq!(envelope.tool, "glass_a11y_marks");
    assert_eq!(envelope.result["count"], json!(1));
    match &out.0[2] {
        OutContent::Text(t) => assert!(t.contains("#1 Button \"Save\""), "legend: {t}"),
        _ => panic!("expected legend text"),
    }
}

#[test]
fn a11y_marks_legend_spells_a_description_apart_from_a_name() {
    use glass_core::{AxRect, Frame};
    let mut tree = fake_tree();
    let mut icon = tree.root.children[0].clone();
    icon.name = None;
    icon.description = Some("Bold".into());
    icon.bounds = Some(AxRect {
        x: 40,
        y: 10,
        width: 20,
        height: 20,
    });
    tree.root.children.push(icon);
    let platform =
        FakePlatform::new(100, 100).with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let mut g = glass_with_a11y(platform, tree);
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let out = a11y_marks(&mut g).unwrap();
    let OutContent::Text(legend) = &out.0[2] else {
        panic!("expected legend text")
    };
    // A real name rides in the quoted slot a `name:` selector matches; a description
    // rides in `desc="…"`, exactly as the outline spells it.
    assert!(legend.contains("#1 Button \"Save\""), "legend: {legend}");
    assert!(
        legend.contains("#2 Button desc=\"Bold\""),
        "legend: {legend}"
    );
    assert!(
        !legend.contains("Button \"Bold\""),
        "a description must never render as a name: {legend}"
    );
}

#[test]
fn a11y_marks_legend_untrusted_wrapped_and_image_note_present() {
    use glass_core::Frame;
    let platform =
        FakePlatform::new(100, 100).with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let mut g = glass_with_a11y(platform, fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let out = a11y_marks(&mut g).unwrap();
    // [Image, envelope-Text, legend-Text (untrusted-wrapped), IMAGE_NOTE-Text]
    assert!(
        out.0.len() >= 4,
        "expected [Image, envelope, legend, IMAGE_NOTE], got {} items",
        out.0.len()
    );
    assert!(
        matches!(out.0[0], OutContent::Image(_)),
        "image leads: {:?}",
        out.0
    );
    // the trusted envelope comes right after the image
    match &out.0[1] {
        OutContent::Envelope(envelope) => {
            assert_eq!(envelope.tool, "glass_a11y_marks");
        }
        _ => panic!("expected envelope as second item"),
    }
    // legend must be untrusted-wrapped
    match &out.0[2] {
        OutContent::Text(t) => {
            assert!(
                t.starts_with(crate::untrusted::NOTE),
                "legend must start with NOTE: {t}"
            );
            assert!(
                t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"),
                "legend must be untrusted-wrapped: {t}"
            );
            assert!(
                t.contains("#1 Button"),
                "legend must still contain element: {t}"
            );
        }
        _ => panic!("expected legend text as third item"),
    }
    // IMAGE_NOTE must be present
    let has_note = out
        .0
        .iter()
        .any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
    assert!(has_note, "IMAGE_NOTE must be present in a11y_marks output");
}

pub(crate) fn started_a11y_frames(frames: Vec<glass_core::Frame>) -> Glass {
    let mut g = glass_with_a11y(FakePlatform::new(100, 100).with_frames(frames), fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap(); // populate last_ax for click_element/set_value
    g
}

#[test]
fn return_none_is_confirmation_only() {
    let mut g = started_a11y_frames(vec![]);
    let out = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: None,
        },
    )
    .unwrap();
    assert_eq!(out.0.len(), 1, "just the envelope, no siblings");
    let v = assert_envelope(&out, "glass_click_element");
    assert_eq!(v["id"], json!(1), "envelope: {v}");
    assert!(v["observed"].is_null(), "envelope: {v}");
    // The fake's default invoke is unsupported (trait default), so the pointer
    // path ran and the fallback reason must be disclosed.
    assert_eq!(v["method"], json!("pointer"), "envelope: {v}");
    assert!(
        v["native_fallback"].as_str().is_some_and(|s| !s.is_empty()),
        "envelope: {v}"
    );

    let out2 = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: Some("none".into()),
        },
    )
    .unwrap();
    assert_eq!(out2.0.len(), 1);
}

#[test]
fn click_element_discloses_native_action_with_no_fallback() {
    let mut g = glass_with_a11y_invoke_ok(FakePlatform::new(100, 100), fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    let out = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: None,
        },
    )
    .unwrap();
    let v = assert_envelope(&out, "glass_click_element");
    assert_eq!(v["method"], json!("native-action"), "envelope: {v}");
    assert!(
        v.get("native_fallback").is_none(),
        "no fallback key on the native-action path: {v}"
    );
}

#[test]
fn click_element_names_the_element_it_actuated_instead() {
    // The backend resolved the click onto a different element. Without this key the
    // result cannot distinguish "clicked the label" from "clicked the row around it".
    let mut g = glass_with_a11y_invoke_on_another(FakePlatform::new(100, 100), fake_tree(), 7);
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    let out = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: None,
        },
    )
    .unwrap();
    let v = assert_envelope(&out, "glass_click_element");
    assert_eq!(v["method"], json!("native-action"), "envelope: {v}");
    assert_eq!(v["id"], json!(1), "envelope: {v}");
    assert_eq!(v["actuated_id"], json!(7), "envelope: {v}");
}

#[test]
fn click_element_omits_actuated_id_when_the_target_itself_was_clicked() {
    let mut g = glass_with_a11y_invoke_ok(FakePlatform::new(100, 100), fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    let out = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: None,
        },
    )
    .unwrap();
    let v = assert_envelope(&out, "glass_click_element");
    assert!(
        v.get("actuated_id").is_none(),
        "nothing was substituted: {v}"
    );
}

#[test]
fn return_unknown_errors() {
    let mut g = started_a11y_frames(vec![]);
    let err = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: Some("wat".into()),
        },
    )
    .unwrap_err();
    assert!(err.contains("unknown return"), "msg: {err}");
}

#[test]
fn return_snapshot_appends_tree_and_refreshes_cache() {
    let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let out = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: Some("snapshot".into()),
        },
    )
    .unwrap();
    assert_eq!(
        out.0.len(),
        2,
        "envelope + exactly one sibling (the a11y outline)"
    );
    let v = assert_envelope(&out, "glass_click_element");
    assert_eq!(v["id"], json!(1), "envelope: {v}");
    assert!(
        v["observed"].is_null(),
        "snapshot doesn't populate `observed`: {v}"
    );
    match &out.0[1] {
        OutContent::Text(t) => {
            assert!(
                t.starts_with(crate::untrusted::NOTE),
                "must be marked untrusted: {t}"
            );
            assert!(
                t.contains("#1 Button \"Save\""),
                "a11y outline appended: {t}"
            );
        }
        _ => panic!("expected a11y outline text"),
    }
    // the snapshot refreshed last_ax -> a follow-up id-based action still resolves
    assert!(
        click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: None
            }
        )
        .is_ok()
    );
}

#[test]
fn return_snapshot_discloses_an_unpublished_document_the_same_way_a_snapshot_does() {
    // The fold's steer wiring, not `a11y_steers` itself: every fold test until now used a
    // tree with nothing to disclose, so dropping the `extend` broke nothing.
    let mut g = glass_with_a11y(
        FakePlatform::new(100, 100).with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]),
        unpublished_document_tree(),
    );
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    // Populates the id cache click_element resolves against, and is the parity tree.
    let tree = g.a11y_snapshot(None).unwrap();
    let out = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: Some("snapshot".into()),
        },
    )
    .unwrap();
    let texts: Vec<&str> = out
        .0
        .iter()
        .filter_map(|c| match c {
            OutContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let steer = texts
        .iter()
        .find(|t| t.contains("has no readable content"))
        .unwrap_or_else(|| panic!("the fold owes the document guidance: {texts:?}"));
    assert!(
        !steer.starts_with(crate::untrusted::NOTE) && !steer.contains("⟦untrusted:"),
        "glass's own guidance, outside the untrusted envelope: {steer}"
    );
    assert!(steer.contains("glass_screenshot"), "{steer}");
    // Parity with `a11y_snapshot`: a fifth steer added to one call site and not the
    // other fails here too.
    for expected in a11y_steers(&tree) {
        assert!(
            texts.iter().any(|t| **t == expected),
            "the fold dropped a steer: {expected}"
        );
    }
}

#[test]
fn return_snapshot_settles_before_folding() {
    use std::sync::{Arc, Mutex};
    // A settleable frame + a capture counter, wired inline (started_a11y_frames doesn't
    // expose a capture log).
    let captures = Arc::new(Mutex::new(0usize));
    let platform = FakePlatform::new(100, 100)
        .with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])])
        .with_capture_log(captures.clone());
    let mut g = glass_with_a11y(platform, fake_tree());
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    a11y_snapshot(&mut g, &A11ySnapshotArgs { max_nodes: None }).unwrap(); // seed last_ax for click_element
    let before = *captures.lock().unwrap();
    let out = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: Some("snapshot".into()),
        },
    )
    .unwrap();
    // The a11y outline is still folded (envelope + one untrusted sibling) ...
    assert_eq!(out.0.len(), 2, "envelope + a11y outline sibling");
    // ... AND the settle captured frames before the fold. This guards the `wait_stable`
    // line: remove it and `captures` stays at `before`.
    assert!(
        *captures.lock().unwrap() > before,
        "return:snapshot must settle (capture frames) before folding"
    );
}

#[test]
fn return_snapshot_without_frames_propagates_settle_failure() {
    let mut g = started_a11y_frames(vec![]);
    let error = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: Some("snapshot".into()),
        },
    )
    .unwrap_err();
    assert!(
        error.contains("capture failed: no scripted frames"),
        "{error}"
    );
}

#[test]
fn return_settle_appends_settled_text() {
    // wait_stable needs frames; one solid frame (repeated by the fake) settles.
    let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let out = click_element(
        &mut g,
        &ClickElementArgs {
            id: 1,
            return_: Some("settle".into()),
        },
    )
    .unwrap();
    assert_eq!(
        out.0.len(),
        1,
        "settle folds into `result.observed`, no extra sibling"
    );
    let v = assert_envelope(&out, "glass_click_element");
    assert_eq!(v["id"], json!(1), "envelope: {v}");
    assert_eq!(v["observed"]["settled"], json!(true), "envelope: {v}");
}

#[test]
fn set_value_return_snapshot() {
    let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let out = set_value(
        &mut g,
        &SetValueArgs {
            id: 1,
            text: "x".into(),
            return_: Some("snapshot".into()),
        },
    )
    .unwrap();
    let v = assert_envelope(&out, "glass_set_value");
    assert_eq!(v["id"], json!(1), "envelope: {v}");
    assert!(
        matches!(&out.0[1], OutContent::Text(t) if t.starts_with(crate::untrusted::NOTE) && t.contains("#1 Button")),
        "outline appended"
    );
}

#[test]
fn set_value_return_settle_folds_into_observed() {
    // Mirrors `return_settle_appends_settled_text` for `click_element`: wait_stable
    // needs frames; one solid frame (repeated by the fake) settles.
    let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let out = set_value(
        &mut g,
        &SetValueArgs {
            id: 1,
            text: "x".into(),
            return_: Some("settle".into()),
        },
    )
    .unwrap();
    assert_eq!(
        out.0.len(),
        1,
        "settle folds into `result.observed`, no extra sibling"
    );
    let v = assert_envelope(&out, "glass_set_value");
    assert_eq!(v["id"], json!(1), "envelope: {v}");
    assert_eq!(v["observed"]["settled"], json!(true), "envelope: {v}");
}

#[test]
fn type_return_settle_folds_into_observed() {
    // Mirrors the click_element/set_value settle observes: wait_stable needs frames;
    // one solid frame (repeated by the fake) settles.
    let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let out = type_text(
        &mut g,
        &TypeArgs {
            text: "hi".into(),
            return_: Some("settle".into()),
        },
    )
    .unwrap();
    assert_eq!(
        out.0.len(),
        1,
        "settle folds into `result.observed`, no extra sibling"
    );
    let v = assert_envelope(&out, "glass_type");
    assert_eq!(v["observed"]["settled"], json!(true), "envelope: {v}");
}

#[test]
fn type_return_snapshot_appends_outline() {
    let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
    let out = type_text(
        &mut g,
        &TypeArgs {
            text: "x".into(),
            return_: Some("snapshot".into()),
        },
    )
    .unwrap();
    assert_eq!(
        out.0.len(),
        2,
        "envelope + exactly one sibling (the a11y outline)"
    );
    assert_envelope(&out, "glass_type");
    assert!(
        matches!(&out.0[1], OutContent::Text(t) if t.starts_with(crate::untrusted::NOTE) && t.contains("#1 Button")),
        "outline appended"
    );
    // the snapshot refreshed last_ax -> a follow-up id-based action still resolves
    assert!(
        click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: None
            }
        )
        .is_ok()
    );
}

#[test]
fn type_unknown_return_rejected_before_any_keystroke() {
    use std::sync::{Arc, Mutex};
    // A bad `return` value must fail BEFORE the text is injected — an agent that
    // retries after this error must not end up with the text typed twice.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut g = glass_with(FakePlatform::new(100, 100).with_event_log(log.clone()));
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let err = type_text(
        &mut g,
        &TypeArgs {
            text: "x".into(),
            return_: Some("bogus".into()),
        },
    )
    .unwrap_err();
    assert!(err.contains("unknown return"), "msg: {err}");
    assert!(
        log.lock().unwrap().is_empty(),
        "no input injected on a rejected `return`: {:?}",
        log.lock().unwrap()
    );
}

#[test]
fn type_observe_failure_says_text_was_typed() {
    use std::sync::{Arc, Mutex};
    // A runtime observe failure (here: `snapshot` on a session with no a11y reader)
    // happens AFTER the keystrokes landed. The error must say so, or an agent
    // retries the whole call and the field ends up with the text twice.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut g = glass_with(FakePlatform::new(100, 100).with_event_log(log.clone()));
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();
    let err = type_text(
        &mut g,
        &TypeArgs {
            text: "hi".into(),
            return_: Some("snapshot".into()),
        },
    )
    .unwrap_err();
    assert!(err.contains("text was typed"), "msg: {err}");
    assert_eq!(*log.lock().unwrap(), vec!["type(hi)"], "keystrokes landed");
}

#[test]
fn list_and_select_window_tools() {
    let mut g = glass_with(FakePlatform::new(320, 240));
    g.start(&AppSpec {
        build: None,
        run: vec!["x".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Off,
        a11y: false,
    })
    .unwrap();

    let out = list_windows(&mut g).unwrap();
    let v = assert_envelope(&out, "glass_list_windows");
    assert_eq!(v["count"], json!(1), "envelope: {v}");
    let text = match &out.0[1] {
        OutContent::Text(t) => t.clone(),
        _ => panic!("expected text"),
    };
    assert!(
        text.starts_with(crate::untrusted::NOTE),
        "must be marked untrusted: {text}"
    );
    assert!(
        text.contains("⟦untrusted:") && text.contains("⟦/untrusted:"),
        "enveloped: {text}"
    );
    assert!(
        text.contains("\"id\":0"),
        "json should list window id 0: {text}"
    );
    assert!(
        text.contains("\"active\":true"),
        "json should mark active: {text}"
    );
    assert!(
        text.contains("\"width\":320"),
        "json should include geometry width: {text}"
    );

    let out = select_window(&mut g, &SelectWindowArgs { id: 0 }).unwrap();
    let v = assert_envelope(&out, "glass_select_window");
    assert_eq!(v["width"], json!(320), "envelope: {v}");
    assert_eq!(v["height"], json!(240), "envelope: {v}");
    assert!(select_window(&mut g, &SelectWindowArgs { id: 42 }).is_err());
}

#[test]
fn result_envelope_is_leading_and_shaped() {
    let out = ToolOutput::result("glass_stop", serde_json::json!({}));
    let OutContent::Envelope(envelope) = &out.0[0] else {
        panic!("expected envelope")
    };
    assert_eq!(envelope.tool, "glass_stop");
    assert_eq!(envelope.result, serde_json::json!({}));
}

#[test]
fn result_with_puts_envelope_first_then_extra() {
    let out = ToolOutput::result_with(
        "glass_screenshot",
        serde_json::json!({ "width": 4, "height": 4 }),
        vec![OutContent::Image(vec![1, 2, 3])],
    );
    assert!(
        matches!(out.0[0], OutContent::Envelope(_)),
        "envelope leads"
    );
    assert!(matches!(out.0[1], OutContent::Image(_)), "extra follows");
}

#[test]
fn production_application_text_conduits_have_explicit_trust_roles() {
    use glass_core::Stream;

    fn start(glass: &mut Glass) {
        glass
            .start(&AppSpec {
                build: None,
                run: vec!["app".into()],
                cwd: None,
                env: vec![],
                window_hint: None,
                timeout_ms: 1,
                sandbox: SandboxLevel::Off,
                a11y: true,
            })
            .unwrap();
    }

    fn text_roles(output: &ToolOutput) -> Vec<(crate::output::TextTrust, crate::output::TextRole)> {
        output
            .0
            .iter()
            .filter_map(|content| match content {
                OutContent::Text(text) => Some((text.trust, text.role)),
                _ => None,
            })
            .collect()
    }

    let stable = vec![Frame::solid(100, 100, [0, 0, 0, 255]); 5];
    let mut semantic =
        glass_with_a11y(FakePlatform::new(100, 100).with_frames(stable), fake_tree());
    start(&mut semantic);
    let snapshot = a11y_snapshot(&mut semantic, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    let automatic = click_element(
        &mut semantic,
        &ClickElementArgs {
            id: 1,
            return_: Some("snapshot".into()),
        },
    )
    .unwrap();
    let find = find_elements(
        &mut semantic,
        &FindElementsArgs {
            query: Some("Save".into()),
            role: None,
            states: None,
            within: None,
            max_results: None,
            max_nodes: None,
            timeout_ms: None,
        },
    )
    .unwrap();
    let marks = a11y_marks(&mut semantic).unwrap();
    let wait_element = wait_for_element(
        &mut semantic,
        &WaitForElementArgs {
            name: Some("Save".into()),
            description: None,
            role: None,
            condition: None,
            value: None,
            value_contains: None,
            interval_ms: Some(0),
            timeout_ms: Some(1000),
        },
    )
    .unwrap();

    let mut ordinary = glass_with(
        FakePlatform::new(100, 100).with_logs(vec![(Stream::Stdout, "application log")]),
    );
    start(&mut ordinary);
    let logs = logs(
        &mut ordinary,
        &LogsArgs {
            cursor: None,
            max_lines: None,
            stream: None,
            contains: None,
        },
    )
    .unwrap();
    clipboard_set(
        &mut ordinary,
        &ClipboardSetArgs {
            text: "application clipboard".into(),
        },
    )
    .unwrap();
    let clipboard = clipboard_get(&mut ordinary).unwrap();
    let windows = list_windows(&mut ordinary).unwrap();
    let mut waiting = glass_with(
        FakePlatform::new(100, 100).with_logs(vec![(Stream::Stdout, "application wait log")]),
    );
    start(&mut waiting);
    let wait_log = wait_for_log(
        &mut waiting,
        &WaitForLogArgs {
            contains: "application".into(),
            stream: None,
            cursor: Some(0),
            interval_ms: Some(0),
            timeout_ms: Some(1000),
        },
    )
    .unwrap();

    let mut failed = glass_with_a11y_invoke_error(
        FakePlatform::new(100, 100),
        fake_tree(),
        "application batch detail",
    );
    start(&mut failed);
    a11y_snapshot(&mut failed, &A11ySnapshotArgs { max_nodes: None }).unwrap();
    let batch_error = do_actions(
        &mut failed,
        &DoArgs {
            actions: vec![Action::ClickElement(ClickElementArgs {
                id: 1,
                return_: None,
            })],
            then: None,
            timeout_ms: None,
            encoded_argument_bytes: 0,
        },
    )
    .unwrap_err();

    use crate::output::{TextRole, TextTrust};
    let untrusted_observation = vec![(TextTrust::UntrustedApplication, TextRole::Observation)];
    for (name, output, expected) in [
        (
            "explicit accessibility snapshot",
            snapshot,
            untrusted_observation.clone(),
        ),
        (
            "automatic return snapshot",
            automatic,
            untrusted_observation.clone(),
        ),
        ("find", find, untrusted_observation.clone()),
        ("logs", logs, untrusted_observation.clone()),
        ("clipboard get", clipboard, untrusted_observation.clone()),
        ("list windows", windows, untrusted_observation.clone()),
        (
            "accessibility marks",
            marks,
            vec![
                (TextTrust::UntrustedApplication, TextRole::Observation),
                (TextTrust::Trusted, TextRole::Guidance),
            ],
        ),
        ("wait element", wait_element, untrusted_observation.clone()),
        ("wait log", wait_log, untrusted_observation),
        (
            "batch error detail",
            batch_error,
            vec![
                (TextTrust::Trusted, TextRole::ErrorDetail),
                (TextTrust::UntrustedApplication, TextRole::Observation),
            ],
        ),
    ] {
        assert_eq!(text_roles(&output), expected, "{name}");
    }
}
