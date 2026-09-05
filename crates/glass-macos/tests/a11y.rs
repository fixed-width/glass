//! Mac-gated accessibility-reader integration test — the first real-AX-tree proof through
//! the whole `glass-a11y-macos` snapshot + set_value + invoke path (`MacosPlatform::start_app`
//! -> `AxContext` -> `MacosA11y::snapshot`/`set_value`/`invoke` -> AXUIElement walk ->
//! `AxTree`), driven against the `a11y_fixture` Cocoa app (a "Save" button, an "Enable"
//! checkbox, an "Active" NSSwitch beside it — AppKit reports the two differently, so the pair
//! is what proves the reader consults the subrole — a "Bold" button whose accessibility label
//! differs from its title, an editable
//! "Note" field — labeled "Note" via `setAccessibilityLabel`, holding the content "hello" —
//! and a non-interactive "Status" label). Four of them carry a deliberate `AXHelp`/`AXTitle`
//! arrangement, so the snapshot checks pin which attribute the reader's `description` comes
//! from. After the snapshot checks,
//! it round-trips `set_value` on the "Note" field ("hello" -> "world") and confirms the
//! non-editable "Save" button rejects a write with `AxElementNotEditable`.
//!
//! Then, a **native-invoke** check: `invoke` fires `AXPress` on the "Save" button and
//! confirms the fixture's own `onSave` handler ran (via the same `SAVE_CLICKED` marker the
//! bounds-agreement check below also uses — AXPress runs the identical target/action a real
//! click does, so reusing the marker is a stronger proof, not a weaker one), then confirms
//! the non-actionable "Status" label rejects `invoke` with `AxActionUnavailable`.
//!
//! Finally, a **bounds-agreement drift guard**: it clamped-centers the "Save" node's
//! a11y-reported `bounds` into window-relative pixels and dispatches a real
//! `MacosPlatform::send_pointer(Click)` there (the exact same input path `tests/input.rs`
//! exercises), then confirms the fixture's *own* button handler — not the a11y layer —
//! printed `SAVE_CLICKED`. `MacosA11y::snapshot`'s bounds (AXUIElement's `kAXPositionAttribute`/
//! `kAXSizeAttribute`, converted to window-relative pixels) and `MacosPlatform::send_pointer`'s
//! injection (window-relative pixel -> global point -> `CGEvent`) are two independent
//! coordinate pipelines; this is the end-to-end proof they agree, closing the loop the unit
//! tests around each side can only assert in isolation.
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "a11y"` entry): like
//! `capture.rs`/`input.rs`/`windows.rs`, `MacosPlatform::start_app` reaches
//! `ffi::app_kit_init()` -> `NSApplication::sharedApplication(mtm)`, which requires the
//! process's TRUE main thread. libtest runs every `#[test]` on a worker thread, so this file
//! defines its own `fn main()` that — run directly rather than through libtest — is on the
//! real main thread. `MacosA11y::snapshot` itself runs inline on that same thread (AX has no
//! separate thread-affinity requirement).
//!
//! Needs the Accessibility (and Screen Recording, for `MacosPlatform::new`'s preflight) TCC
//! grants, which only the signed, granted `GlassProbe.app` bundle holds on this project's
//! dev Mac — see `capture.rs`'s module doc and `scripts/test-macos.sh` for how the
//! granted run copies this binary into that bundle. The fixture binary path is taken from
//! `GLASS_A11Y_FIXTURE_BIN` when set (the granted run pre-builds it); otherwise this builds
//! `fixture/a11y_fixture.swift` with `swiftc`, or skips if neither is available.

mod common;

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("skipped (not macOS): test");
}

#[cfg(target_os = "macos")]
fn main() {
    macos_main::run();
}

#[cfg(target_os = "macos")]
mod macos_main {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use glass_core::platform::{MouseButton, PointerEvent};
    use glass_core::{
        Accessibility, AppSpec, AxContext, AxNode, AxRole, AxTarget, Backend, BaselineStore,
        Deadline, Glass, GlassError, Platform, PlatformFactory, SandboxLevel, Stream, WalkLimits,
    };
    use glass_macos::MacosPlatform;

    use crate::common::{build_fixture, fail, swiftc_available, try_expect};

    /// Settle after an action that should print `SAVE_CLICKED` — native `invoke` or the
    /// a11y-bounds pointer click — before draining logs, mirroring `input.rs`'s
    /// `ACTION_SETTLE`. Generous relative to the action's own internal focus-settle so the
    /// fixture's `fflush`ed line has definitely been read by the platform's background log
    /// reader before we drain.
    const CLICK_SETTLE: Duration = Duration::from_millis(400);

    /// Four of the fixture's element lines, asserted as substrings of the tree outline.
    /// `to_outline` renders each node as `#<id> <Role> "<name>" ...`, rendering `name` and never
    /// `value` — so the editable field's stable label (`setAccessibilityLabel("Note")`,
    /// surfaced as `AXDescription`) is what appears here, not its volatile content ("hello").
    /// The content is checked separately, via [`find_text_field`], against `AxNode::value`.
    const NEEDLES: [&str; 5] = [
        "Button \"Save\"",
        "CheckBox \"Enable\"",
        // The on-box proof that the reader consults the subrole: without it an NSSwitch renders as
        // Button and this needle is absent.
        "ToggleButton \"Active\"",
        "TextField \"Note\"",
        "Label \"Status\"",
    ];

    /// Pre-order search for the first `TextField` node — the fixture's editable "Note" field.
    /// Separate from the [`NEEDLES`] outline check because `to_outline` renders labels (`name`,
    /// plus `desc` where a reader sources one) and never `value`; this reaches `AxNode::value`
    /// directly to prove content is read independently of the (stable) label.
    fn find_text_field(node: &AxNode) -> Option<&AxNode> {
        if node.role == AxRole::TextField {
            return Some(node);
        }
        node.children.iter().find_map(find_text_field)
    }

    /// Pre-order search for the first node whose `name` is exactly `name` — used to build an
    /// `AxTarget` (id/role/name/bounds) for `set_value`'s round-trip and non-editable checks.
    fn find_by_name<'a>(node: &'a AxNode, name: &str) -> Option<&'a AxNode> {
        if node.name.as_deref() == Some(name) {
            return Some(node);
        }
        node.children.iter().find_map(|c| find_by_name(c, name))
    }

    /// True if any captured stdout line in `lines` contains `needle` — used by the
    /// bounds-agreement check to confirm the fixture's own `onSave` handler (not the a11y
    /// layer) observed the click. Mirrors `input.rs`'s `find_reported`, simplified to a
    /// substring test since the fixture's marker line has no variable payload to parse out.
    fn logs_contain(lines: &[(Stream, String)], needle: &str) -> bool {
        lines
            .iter()
            .any(|(stream, line)| *stream == Stream::Stdout && line.contains(needle))
    }

    fn semantic_target(
        role: AxRole,
        name: &str,
        states: Vec<glass_core::SemanticState>,
    ) -> glass_core::SemanticTarget {
        glass_core::SemanticTarget {
            target: glass_core::SemanticSelector::new(Some(name.into()), Some(role), states)
                .expect("valid semantic selector"),
            within: None,
        }
    }

    fn actionability_verdict(
        report: &glass_core::ActionabilityReport,
        name: glass_core::ActionabilityCheckName,
    ) -> glass_core::ActionabilityVerdict {
        report
            .checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("missing {name:?} check in {:?}", report.checks))
            .verdict
    }

    fn log_cursor(glass: &mut Glass) -> Result<u64, String> {
        glass
            .logs(0, 1_000, None, None)
            .map(|(_, cursor)| cursor)
            .map_err(|error| format!("read fixture log cursor: {error}"))
    }

    fn exact_log_count_since(
        glass: &mut Glass,
        cursor: u64,
        expected: &str,
    ) -> Result<usize, String> {
        glass
            .logs(cursor, 1_000, None, None)
            .map(|(lines, _)| lines.iter().filter(|line| line.text == expected).count())
            .map_err(|error| format!("read fixture logs: {error}"))
    }

    fn await_exact_log_arrival(
        glass: &mut Glass,
        cursor: u64,
        expected: &str,
        count: usize,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let observed = exact_log_count_since(glass, cursor, expected)?;
            if observed == count {
                return Ok(());
            }
            if observed > count || Instant::now() >= deadline {
                return Err(format!(
                    "expected exactly {count} {expected:?} logs, got {observed}"
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn await_exact_log_count(
        glass: &mut Glass,
        cursor: u64,
        expected: &str,
        count: usize,
    ) -> Result<(), String> {
        if count != 0 {
            await_exact_log_arrival(glass, cursor, expected, count)?;
        }
        let quiet_until = Instant::now() + Duration::from_millis(400);
        loop {
            let observed = exact_log_count_since(glass, cursor, expected)?;
            if observed != count {
                return Err(format!(
                    "expected {count} exact {expected:?} logs throughout the quiet window, got {observed}"
                ));
            }
            if Instant::now() >= quiet_until {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn glass_with_a11y() -> Glass {
        let factory: PlatformFactory = Box::new(|_backend| {
            Ok(Backend {
                platform: Box::new(MacosPlatform::new()?),
                accessibility: Some(Box::new(glass_a11y_macos::MacosA11y::new())),
            })
        });
        let dir = tempfile::tempdir().expect("semantic baseline tempdir");
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        Glass::new(factory, "macos".into(), BaselineStore::new(root), 100)
    }

    /// Launch the fixture, snapshot its accessibility tree, and assert the outline contains
    /// each of [`NEEDLES`]. Returns `Err` instead of exiting so `run()` can always reach
    /// `stop_app` first (a bare `process::exit` here would skip `MacosPlatform::Drop` and
    /// leak the spawned fixture — same rationale as `capture.rs::run_checks`).
    fn run_checks(
        platform: &mut MacosPlatform,
        fixture_bin: &std::path::Path,
    ) -> Result<(), String> {
        let spec = AppSpec {
            build: None,
            run: vec![fixture_bin.to_string_lossy().into_owned()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 8000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        };

        let geometry = try_expect(platform.start_app(&spec), "start_app")?;
        println!("started fixture window: {geometry:?}");

        // start_app only waits for the window to exist, not for AppKit to finish building
        // the accessibility tree behind it — give it a moment to settle before snapshotting.
        std::thread::sleep(Duration::from_millis(800));

        let ctx = AxContext {
            pids: platform.app_pids(),
            window: geometry.clone(),
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: Deadline::UNBOUNDED,
        };

        let mut a11y = glass_a11y_macos::MacosA11y::new();
        let mut tree = try_expect(a11y.snapshot(&ctx), "snapshot")?;
        tree.assign_ids(); // number nodes so the diagnostic outline reads naturally
        let outline = tree.to_outline();
        println!("a11y snapshot ({} nodes):\n{outline}", tree.count);

        for needle in NEEDLES {
            if !outline.contains(needle) {
                return Err(format!("missing {needle} in outline:\n{outline}"));
            }
        }

        // `raw_role` must be the platform's own AX role token, not `AXRoleDescription`'s
        // localized human phrase ("button" / "bouton"): the fixture's Save button is an
        // NSButton, which reports exactly `AXButton` on every machine and every locale.
        let save_raw = match find_by_name(&tree.root, "Save") {
            Some(n) => n,
            None => return Err(format!("no \"Save\" button in tree:\n{outline}")),
        };
        if save_raw.raw_role != "AXButton" {
            return Err(format!(
                "Save raw_role = {:?}, want \"AXButton\":\n{outline}",
                save_raw.raw_role
            ));
        }

        // The secondary label, one case per rule. The census a probe run prints can only say that
        // *something* arrived; these say which attribute produced it, and that the two cases which
        // must produce nothing do.
        for (name, want) in [
            // `AXHelp`, distinct from the name.
            ("Save", Some("Saves and closes the sheet")),
            // `AXTitle` named it, so `AXDescription` is free to describe it.
            ("Bold", Some("Bold text style")),
            // Labelled but untitled: `AXDescription` IS the name, and must not be repeated.
            ("Note", None),
            // `AXHelp` identical to the name: dropped, not printed twice.
            ("Status", None),
        ] {
            let node = match find_by_name(&tree.root, name) {
                Some(n) => n,
                None => return Err(format!("no {name:?} node in tree:\n{outline}")),
            };
            if node.description.as_deref() != want {
                return Err(format!(
                    "{name:?} description = {:?}, want {want:?}:\n{outline}",
                    node.description
                ));
            }
        }

        // The outline only proves `name`; confirm `value` is read separately, straight off
        // the field's content — not folded into `name` (that was the bug this test guards).
        let field = match find_text_field(&tree.root) {
            Some(n) => n,
            None => return Err(format!("no TextField node in tree:\n{outline}")),
        };
        if field.value != Some("hello".to_string()) {
            return Err(format!(
                "TextField value = {:?}, want Some(\"hello\"):\n{outline}",
                field.value
            ));
        }

        // Round-trip an editable field: "hello" -> "world" via set_value, then re-snapshot
        // and confirm the field's value actually changed (not a silent no-op).
        let note = match find_by_name(&tree.root, "Note") {
            Some(n) => n,
            None => return Err(format!("no \"Note\" field in tree:\n{outline}")),
        };
        let note_tgt = AxTarget {
            id: note.id,
            role: note.role,
            name: note.name.clone(),
            bounds: note.bounds,
            value: note.value.clone(),
        };
        try_expect(
            a11y.set_value(&ctx, &note_tgt, "world"),
            "set_value(Note, \"world\")",
        )?;

        let mut tree2 = try_expect(a11y.snapshot(&ctx), "re-snapshot after set_value")?;
        tree2.assign_ids();
        let outline2 = tree2.to_outline();
        let field2 = match find_text_field(&tree2.root) {
            Some(n) => n,
            None => return Err(format!("no TextField node in re-snapshot:\n{outline2}")),
        };
        if field2.value != Some("world".to_string()) {
            return Err(format!(
                "TextField value after set_value = {:?}, want Some(\"world\"):\n{outline2}",
                field2.value
            ));
        }

        // A button is not editable: set_value must reject it, not silently no-op.
        let save = match find_by_name(&tree2.root, "Save") {
            Some(n) => n,
            None => return Err(format!("no \"Save\" button in re-snapshot:\n{outline2}")),
        };
        let save_tgt = AxTarget {
            id: save.id,
            role: save.role,
            name: save.name.clone(),
            bounds: save.bounds,
            value: save.value.clone(),
        };
        match a11y.set_value(&ctx, &save_tgt, "x") {
            Err(GlassError::AxElementNotEditable(_)) => {}
            other => {
                return Err(format!(
                    "expected AxElementNotEditable for Save, got {other:?}"
                ));
            }
        }

        println!("A11Y_SETVALUE_PASS");

        // --- Native invoke: fire AXPress on the "Save" button through `invoke` and confirm
        // the fixture's own `onSave` handler ran — reusing SAVE_CLICKED (rather than a
        // dedicated marker) since AXPress on an NSButton runs the identical target/action a
        // real click does, and nothing has printed it yet at this point in the run. ---
        try_expect(a11y.invoke(&ctx, &save_tgt), "invoke(Save)")?;
        std::thread::sleep(CLICK_SETTLE);
        let invoke_logs = platform.drain_logs();
        if !logs_contain(&invoke_logs, "SAVE_CLICKED") {
            return Err(format!(
                "invoke(Save) did not fire the button's action (fixture stdout: {invoke_logs:?})"
            ));
        }

        // A label exposes no AXPress: invoke must report AxActionUnavailable, not silently
        // no-op or fall back.
        let status = match find_by_name(&tree2.root, "Status") {
            Some(n) => n,
            None => return Err(format!("no \"Status\" label in re-snapshot:\n{outline2}")),
        };
        let status_tgt = AxTarget {
            id: status.id,
            role: status.role,
            name: status.name.clone(),
            bounds: status.bounds,
            value: status.value.clone(),
        };
        match a11y.invoke(&ctx, &status_tgt) {
            Err(GlassError::AxActionUnavailable(_)) => {}
            other => {
                return Err(format!(
                    "expected AxActionUnavailable for Status, got {other:?}"
                ));
            }
        }

        println!("A11Y_INVOKE_PASS");

        // --- Bounds-agreement drift guard: clamped-center Save's a11y bounds into
        // window-relative pixels, click there through the real input path, and confirm
        // the fixture's OWN click handler (not the a11y layer) saw it. This proves
        // `MacosA11y::snapshot`'s bounds and `MacosPlatform::send_pointer`'s coordinate
        // system agree end-to-end, not just that each is internally self-consistent. ---
        let save_bounds = match save.bounds {
            Some(b) => b,
            None => {
                return Err(format!(
                    "\"Save\" node has no bounds in re-snapshot:\n{outline2}"
                ));
            }
        };
        let (cx, cy) = match save_bounds.clamped_center(ctx.window.width, ctx.window.height) {
            Some(p) => p,
            None => {
                return Err(format!(
                    "\"Save\" bounds {save_bounds:?} have zero area against window {:?}",
                    ctx.window
                ));
            }
        };
        let click_event = PointerEvent::Click {
            x: cx,
            y: cy,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        };
        try_expect(
            platform.send_pointer(&click_event),
            "send_pointer(Click) on Save's a11y bounds",
        )?;
        std::thread::sleep(CLICK_SETTLE);
        let click_logs = platform.drain_logs();
        if !logs_contain(&click_logs, "SAVE_CLICKED") {
            return Err(format!(
                "click via a11y bounds ({cx},{cy}) did not hit Save (fixture stdout: {click_logs:?})"
            ));
        }
        println!("A11Y_BOUNDS_PASS");

        Ok(())
    }

    fn run_semantic_checks(fixture_bin: &std::path::Path) -> Result<(), String> {
        use glass_core::{
            ActionMethod, ActionMode, ActionTarget, ActionabilityCheckName, ActionabilityVerdict,
            ConfirmationStatus, DispatchStatus, SemanticActionFailureKind, SemanticState,
        };

        let mut glass = glass_with_a11y();
        glass
            .start(&AppSpec {
                build: None,
                run: vec![fixture_bin.to_string_lossy().into_owned()],
                cwd: None,
                env: vec![],
                window_hint: None,
                timeout_ms: 8_000,
                sandbox: SandboxLevel::Off,
                a11y: false,
            })
            .map_err(|error| format!("semantic fixture start: {error}"))?;

        let result = (|| {
            await_exact_log_arrival(&mut glass, 0, "MOVING_SETTLED", 1)?;
            let initial = glass
                .a11y_snapshot(None)
                .map_err(|error| format!("initial semantic snapshot: {error}"))?;
            let initial_moving = find_by_name(&initial.root, "Moving semantic")
                .ok_or_else(|| "initial snapshot has no Moving semantic button".to_string())?;
            let moving_id = initial_moving.id;
            let stale_cached_bounds = initial_moving.bounds;
            let coverage =
                glass_core::Accessibility::state_coverage(&glass_a11y_macos::MacosA11y::new());
            if coverage != glass_a11y_macos::mapping::STATE_COVERAGE {
                return Err(format!("unexpected macOS state coverage: {coverage:?}"));
            }

            let save_cursor = log_cursor(&mut glass)?;
            let native_save = glass
                .click_target(&glass_core::ClickTargetParams {
                    target: ActionTarget::Semantic(semantic_target(
                        AxRole::Button,
                        "Semantic Save",
                        vec![SemanticState::Enabled],
                    )),
                    mode: ActionMode::Native,
                    timeout_ms: Some(5_000),
                    max_nodes: None,
                })
                .map_err(|error| format!("native semantic save: {error}"))?;
            if native_save.action.method != (ActionMethod::NativeAction { actuated: None })
                || native_save.action.dispatch != DispatchStatus::Dispatched
            {
                return Err(format!("unexpected native action report: {native_save:?}"));
            }
            await_exact_log_arrival(&mut glass, save_cursor, "SEMANTIC_SAVE", 1)?;
            await_exact_log_arrival(&mut glass, save_cursor, "MOVING_RESET", 1)?;
            let stale_cursor = log_cursor(&mut glass)?;
            let stale = glass
                .click_element(moving_id)
                .expect_err("moving bounds must stale the cached backend target");
            if !matches!(stale.cause(), GlassError::AxElementChanged(_)) {
                return Err(format!("stale backend rewalk returned {stale}"));
            }
            let stale_current = glass
                .a11y_snapshot(None)
                .map_err(|error| format!("stale rejection evidence snapshot: {error}"))?;
            let stale_current_bounds = find_by_name(&stale_current.root, "Moving semantic")
                .ok_or_else(|| "stale evidence has no Moving semantic button".to_string())?
                .bounds;
            if stale_current_bounds == stale_cached_bounds {
                return Err(format!(
                    "stale rejection did not observe changed bounds: cached={stale_cached_bounds:?}, current={stale_current_bounds:?}"
                ));
            }
            await_exact_log_count(&mut glass, stale_cursor, "MOVING_CLICKED", 0)?;
            await_exact_log_count(&mut glass, save_cursor, "SEMANTIC_SAVE", 1)?;
            await_exact_log_count(&mut glass, save_cursor, "MOVING_RESET", 1)?;

            await_exact_log_arrival(&mut glass, save_cursor, "MOVING_SETTLED", 1)?;
            let pointer_baseline = glass
                .a11y_snapshot(None)
                .map_err(|error| format!("settled pointer baseline snapshot: {error}"))?;
            let pointer_baseline_bounds = find_by_name(&pointer_baseline.root, "Moving semantic")
                .ok_or_else(|| "pointer baseline has no Moving semantic button".to_string())?
                .bounds;

            let moving_cursor = log_cursor(&mut glass)?;
            glass
                .click_target(&glass_core::ClickTargetParams {
                    target: ActionTarget::Semantic(semantic_target(
                        AxRole::Button,
                        "Semantic Save",
                        vec![SemanticState::Enabled],
                    )),
                    mode: ActionMode::Native,
                    timeout_ms: Some(5_000),
                    max_nodes: None,
                })
                .map_err(|error| format!("restart movement through native save: {error}"))?;
            await_exact_log_arrival(&mut glass, moving_cursor, "MOVING_STARTED", 1)?;
            let changing_sample = glass
                .a11y_snapshot(None)
                .map_err(|error| format!("changing pointer sample snapshot: {error}"))?;
            let changing_bounds = find_by_name(&changing_sample.root, "Moving semantic")
                .ok_or_else(|| "changing sample has no Moving semantic button".to_string())?
                .bounds;
            if changing_bounds == pointer_baseline_bounds {
                return Err(format!(
                    "moving bounds did not change after restart: {changing_bounds:?}"
                ));
            }
            let moving_started = std::time::Instant::now();
            let moving = glass
                .click_target(&glass_core::ClickTargetParams {
                    target: ActionTarget::Semantic(semantic_target(
                        AxRole::Button,
                        "Moving semantic",
                        vec![SemanticState::Enabled],
                    )),
                    mode: ActionMode::Pointer,
                    timeout_ms: Some(5_000),
                    max_nodes: None,
                })
                .map_err(|error| format!("forced pointer moving semantic: {error}"))?;
            if moving_started.elapsed() < Duration::from_millis(300)
                || moving.action.method
                    != (ActionMethod::Pointer {
                        native_fallback: None,
                    })
                || actionability_verdict(&moving.actionability, ActionabilityCheckName::Stable)
                    != ActionabilityVerdict::Passed
                || actionability_verdict(&moving.actionability, ActionabilityCheckName::NonOccluded)
                    != ActionabilityVerdict::Passed
            {
                return Err(format!("unexpected moving pointer report: {moving:?}"));
            }
            await_exact_log_count(&mut glass, moving_cursor, "MOVING_SETTLED", 1)?;
            await_exact_log_count(&mut glass, moving_cursor, "MOVING_CLICKED", 1)?;
            await_exact_log_count(&mut glass, moving_cursor, "MOVING_CLICKED_SETTLED", 1)?;
            await_exact_log_count(&mut glass, moving_cursor, "MOVING_CLICKED_MOVING", 0)?;
            await_exact_log_count(&mut glass, moving_cursor, "SEMANTIC_SAVE", 1)?;
            let settled_sample = glass
                .a11y_snapshot(None)
                .map_err(|error| format!("settled pointer sample snapshot: {error}"))?;
            let settled_bounds = find_by_name(&settled_sample.root, "Moving semantic")
                .ok_or_else(|| "settled sample has no Moving semantic button".to_string())?
                .bounds;
            if settled_bounds == changing_bounds {
                return Err(format!(
                    "pointer samples never changed before settling: {changing_bounds:?}"
                ));
            }

            let before_type = glass
                .a11y_snapshot(None)
                .map_err(|error| format!("before targeted type snapshot: {error}"))?;
            let before_value = find_by_name(&before_type.root, "Note")
                .and_then(|node| node.value.clone())
                .ok_or_else(|| "before targeted type snapshot has no Note value".to_string())?;
            let typed = glass
                .type_target(
                    &glass_core::TypeTargetParams {
                        target: semantic_target(
                            AxRole::TextField,
                            "Note",
                            vec![SemanticState::Enabled],
                        ),
                        focus_mode: ActionMode::Native,
                        timeout_ms: 5_000,
                        max_nodes: None,
                    },
                    "Z",
                )
                .map_err(|error| format!("native targeted type: {error}"))?;
            let Some(ref focus) = typed.focus else {
                return Err("targeted type returned no focus report".into());
            };
            if focus.confirmation != ConfirmationStatus::FocusConfirmed
                || focus.dispatch != DispatchStatus::Dispatched
                || typed.action.method != ActionMethod::Keyboard
            {
                return Err(format!("unexpected targeted type report: {typed:?}"));
            }
            let typed_tree = glass
                .a11y_snapshot(None)
                .map_err(|error| format!("targeted type snapshot: {error}"))?;
            let typed_note = find_by_name(&typed_tree.root, "Note")
                .ok_or_else(|| "typed snapshot has no Note field".to_string())?;
            let after_value = typed_note
                .value
                .as_deref()
                .ok_or_else(|| "typed Note has no value".to_string())?;
            let inserted_once = after_value.len() == before_value.len() + 1
                && after_value.matches('Z').count() == before_value.matches('Z').count() + 1
                && after_value.replacen('Z', "", 1) == before_value;
            if !typed_note.states.focused || !inserted_once {
                return Err(format!(
                    "targeted type did not insert exactly one Z: before={before_value:?}, after={after_value:?}, node={typed_note:?}"
                ));
            }
            let typed_value = after_value.to_string();
            std::thread::sleep(Duration::from_millis(400));
            let quiet_typed_tree = glass
                .a11y_snapshot(None)
                .map_err(|error| format!("quiet targeted type snapshot: {error}"))?;
            let quiet_typed_note = find_by_name(&quiet_typed_tree.root, "Note")
                .ok_or_else(|| "quiet typed snapshot has no Note field".to_string())?;
            if quiet_typed_note.value.as_deref() != Some(typed_value.as_str()) {
                return Err(format!(
                    "targeted type changed again during the quiet interval: first={typed_value:?}, quiet={:?}",
                    quiet_typed_note.value
                ));
            }

            let disabled_cursor = log_cursor(&mut glass)?;
            let disabled = glass
                .click_target(&glass_core::ClickTargetParams {
                    target: ActionTarget::Semantic(semantic_target(
                        AxRole::Button,
                        "Disabled semantic",
                        vec![],
                    )),
                    mode: ActionMode::Pointer,
                    timeout_ms: Some(0),
                    max_nodes: None,
                })
                .expect_err("disabled semantic target must be refused");
            if disabled.kind != SemanticActionFailureKind::NotActionable
                || disabled.action_dispatch != DispatchStatus::NotDispatched
                || actionability_verdict(&disabled.actionability, ActionabilityCheckName::Enabled)
                    != ActionabilityVerdict::Failed
            {
                return Err(format!("unexpected disabled refusal: {disabled:?}"));
            }
            await_exact_log_count(&mut glass, disabled_cursor, "DISABLED_SEMANTIC", 0)?;

            let duplicate_cursor = log_cursor(&mut glass)?;
            let duplicate = glass
                .click_target(&glass_core::ClickTargetParams {
                    target: ActionTarget::Semantic(semantic_target(
                        AxRole::Button,
                        "Duplicate semantic",
                        vec![],
                    )),
                    mode: ActionMode::Native,
                    timeout_ms: Some(0),
                    max_nodes: None,
                })
                .expect_err("duplicate semantic target must be refused");
            if duplicate.kind != SemanticActionFailureKind::AmbiguousTarget
                || duplicate.action_dispatch != DispatchStatus::NotDispatched
            {
                return Err(format!("unexpected duplicate refusal: {duplicate:?}"));
            }
            await_exact_log_count(&mut glass, duplicate_cursor, "DUPLICATE_SEMANTIC", 0)?;

            let occluded_cursor = log_cursor(&mut glass)?;
            let occluded = glass
                .click_target(&glass_core::ClickTargetParams {
                    target: ActionTarget::Semantic(semantic_target(
                        AxRole::Button,
                        "Occluded semantic",
                        vec![SemanticState::Enabled],
                    )),
                    mode: ActionMode::Pointer,
                    timeout_ms: Some(5_000),
                    max_nodes: None,
                })
                .expect_err("AX hit testing must prove the foreground occluder");
            if occluded.kind != SemanticActionFailureKind::NotActionable
                || occluded.action_dispatch != DispatchStatus::NotDispatched
                || actionability_verdict(
                    &occluded.actionability,
                    ActionabilityCheckName::NonOccluded,
                ) != ActionabilityVerdict::Failed
            {
                return Err(format!("unexpected occlusion refusal: {occluded:?}"));
            }
            await_exact_log_count(&mut glass, occluded_cursor, "OCCLUDED_CLICKED", 0)?;
            await_exact_log_count(&mut glass, occluded_cursor, "OCCLUDER_CLICKED", 0)?;

            println!("A11Y_SEMANTIC_PASS");
            Ok(())
        })();

        let stop_result = glass.stop();
        match (result, stop_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(format!("semantic fixture stop: {error}")),
        }
    }

    pub(super) fn run() {
        // Prefer a pre-built fixture (the granted run supplies `GLASS_A11Y_FIXTURE_BIN`);
        // otherwise build it here, skipping cleanly if `swiftc` is unavailable.
        let (fixture_bin, fixture_dir) = match std::env::var_os("GLASS_A11Y_FIXTURE_BIN") {
            Some(p) => {
                let path = PathBuf::from(p);
                if !path.is_file() {
                    fail(format!(
                        "GLASS_A11Y_FIXTURE_BIN set but not a file: {}",
                        path.display()
                    ));
                }
                (path, None)
            }
            None => {
                if !swiftc_available() {
                    println!("skipped (GLASS_A11Y_FIXTURE_BIN unset and no swiftc)");
                    return;
                }
                let (bin, dir) = build_fixture("a11y_fixture");
                (bin, Some(dir))
            }
        };
        println!("using a11y fixture at {}", fixture_bin.display());

        let cleanup_dir = |dir: &Option<PathBuf>| {
            if let Some(d) = dir {
                let _ = std::fs::remove_dir_all(d);
            }
        };

        let mut platform = match MacosPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                cleanup_dir(&fixture_dir);
                fail(format!(
                    "MacosPlatform::new() (Screen Recording / Accessibility grant missing?): {e}"
                ));
            }
        };

        let result = run_checks(&mut platform, &fixture_bin);

        // Reached on every path and BEFORE any process::exit below: stop_app is idempotent,
        // so this guarantees the fixture process never survives a failed run.
        let stop_result = platform.stop_app();

        match result {
            Ok(()) => {
                if let Err(error) = stop_result {
                    cleanup_dir(&fixture_dir);
                    fail(format!("stop_app: {error}"));
                }
                if let Err(error) = run_semantic_checks(&fixture_bin) {
                    cleanup_dir(&fixture_dir);
                    fail(error);
                }
                cleanup_dir(&fixture_dir);
                println!("A11Y_SNAPSHOT_PASS");
                std::process::exit(0);
            }
            Err(msg) => {
                if let Err(e) = stop_result {
                    eprintln!("(additionally) stop_app failed: {e}");
                }
                cleanup_dir(&fixture_dir);
                fail(msg);
            }
        }
    }
}
