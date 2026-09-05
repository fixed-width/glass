//! End-to-end drive test for the iOS Simulator backend's input + accessibility.
//!
//! Launches a real fixture app on a booted Simulator and drives it through the public
//! `glass_core` seams — `Platform` for input and `Accessibility` for the tree — exactly the
//! path `glass_click`/`glass_type`/`glass_a11y_snapshot`/`glass_set_value` exercise over MCP.
//! It proves the whole chain end to end: a tap issued at a snapshot element's window-pixel
//! center lands on that element (the READY→TAPPED flip), and typed text — both raw
//! `send_key` and the `set_value` clear-then-type sequence — reaches the field.
//!
//! `#[ignore]`d so a plain `cargo test` skips it everywhere: the backend needs
//! `xcrun simctl` + `idb_companion` (macOS + Xcode only), a booted Simulator, and the
//! GlassFixture app from `examples/ios-fixture/`. Run explicitly on such a host:
//!
//! ```sh
//! ./examples/ios-fixture/build.sh
//! GLASS_IOS_APP="$PWD/examples/ios-fixture/build/GlassFixture.app" \
//!   cargo test -p glass-ios --test drive_integration -- --ignored --nocapture
//! ```
//!
//! `GLASS_IOS_APP` must be a `.app` bundle path so `start_app` installs it itself.
//! `GLASS_IOS_UDID` / `GLASS_IOS_DEVICE` / `GLASS_IDB_COMPANION` select the Simulator and the
//! companion binary the same way they do for `glass-mcp`; see `docs/how-to/setup-ios.md`.
//!
//! The fixture exposes four accessibility elements (by `AXUniqueId`): `statusLabel` (shows
//! READY, flips to TAPPED when the button is tapped), `tapButton`, `inputField`, and
//! `echoLabel` (mirrors the field's text, or `(empty)`) — see
//! `examples/ios-fixture/README.md`.
//!
//! [`web_fixture_button_and_field_respond`] drives a different app — the role fixture's `web`
//! tab, a stock `WKWebView` on `examples/web-role-fixture/index.html` — to read whether web
//! content can be driven the same way at all:
//!
//! ```sh
//! ./examples/ios-role-fixture/build.sh
//! GLASS_IOS_ROLE_FIXTURE="$PWD/examples/ios-role-fixture/build/RoleFixture.app" \
//!   cargo test -p glass-ios --test drive_integration -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use glass_core::accessibility::{
    Accessibility, AxContext, AxNode, AxRole, AxTarget, AxTree, WalkLimits,
};
use glass_core::{
    ActionMethod, ActionMode, ActionTarget, ActionabilityCheckName, ActionabilityVerdict, Backend,
    BaselineStore, ClickTargetParams, ConfirmationStatus, Deadline, DispatchStatus, Glass,
    MutationReport, SemanticActionFailureKind, SemanticSelector, SemanticTarget,
    SetValueTargetParams, TypeTargetParams,
};
use glass_core::{AppSpec, KeyEvent, MouseButton, Platform, PointerEvent, SandboxLevel};
use glass_ios::{IosA11y, IosPlatform, SimulatorRegistry};

/// First node (pre-order) whose `name` equals `name`.
fn find_named<'a>(n: &'a AxNode, name: &str) -> Option<&'a AxNode> {
    if n.name.as_deref() == Some(name) {
        return Some(n);
    }
    n.children.iter().find_map(|c| find_named(c, name))
}

/// The `value` of the first node named `name`, if any.
fn named_value(tree: &AxTree, name: &str) -> Option<String> {
    find_named(&tree.root, name).and_then(|n| n.value.clone())
}

/// Whether the echo label's text equals `want`, ignoring ASCII case. iOS
/// sentence-autocapitalizes the leading letter of a fresh field (so "hello" is echoed
/// as "Hello"), which is the Simulator's text behavior, not glass's input; the
/// case-insensitive compare stays exact enough to catch a leftover from a failed clear.
fn echo_is(tree: &AxTree, want: &str) -> bool {
    named_value(tree, "echoLabel").is_some_and(|v| v.eq_ignore_ascii_case(want))
}

/// Window-pixel click center of the first node named `name`.
fn center_of(tree: &AxTree, name: &str, win: &glass_core::WindowGeometry) -> (i32, i32) {
    let node = find_named(&tree.root, name).unwrap_or_else(|| panic!("{name} present in tree"));
    let bounds = node
        .bounds
        .unwrap_or_else(|| panic!("{name} has bounds in the snapshot"));
    bounds
        .clamped_center(win.width, win.height)
        .unwrap_or_else(|| panic!("{name} is on screen"))
}

/// Re-snapshot (settling between attempts) until `pred` holds or the attempts run out,
/// returning the last snapshot either way so the caller asserts against a concrete tree.
/// The app needs a beat to re-render after each input, so a single snapshot can race it.
fn snapshot_until(
    a11y: &mut IosA11y,
    ctx: &AxContext,
    attempts: usize,
    pred: impl Fn(&AxTree) -> bool,
) -> AxTree {
    let mut tree = a11y.snapshot(ctx).expect("snapshot");
    let mut tries = 0;
    while tries < attempts && !pred(&tree) {
        std::thread::sleep(Duration::from_millis(300));
        tree = a11y.snapshot(ctx).expect("snapshot");
        tries += 1;
    }
    tree
}

fn tap(x: i32, y: i32) -> PointerEvent {
    PointerEvent::Click {
        x,
        y,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    }
}

fn semantic_target(query: &str, role: AxRole) -> SemanticTarget {
    SemanticTarget {
        target: SemanticSelector::new(Some(query.to_owned()), Some(role), Vec::new())
            .expect("valid semantic selector"),
        within: None,
    }
}

fn pointer_click(query: &str, role: AxRole, timeout_ms: u64) -> ClickTargetParams {
    ClickTargetParams {
        target: ActionTarget::Semantic(semantic_target(query, role)),
        mode: ActionMode::Pointer,
        timeout_ms: Some(timeout_ms),
        max_nodes: None,
    }
}

fn semantic_fixture_spec(app: String) -> AppSpec {
    AppSpec {
        build: None,
        run: vec![app],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        a11y: true,
    }
}

struct CountingIosPlatform {
    inner: IosPlatform,
    pointer_events: Arc<Mutex<Vec<PointerEvent>>>,
    key_events: Arc<Mutex<Vec<KeyEvent>>>,
}

impl Platform for CountingIosPlatform {
    fn start_app(&mut self, spec: &AppSpec) -> glass_core::Result<glass_core::WindowGeometry> {
        self.inner.start_app(spec)
    }

    fn stop_app_by(&mut self, deadline: Deadline) -> glass_core::Result<()> {
        self.inner.stop_app_by(deadline)
    }

    fn capture_frame_by(
        &mut self,
        region: Option<&glass_core::Region>,
        deadline: Deadline,
    ) -> glass_core::Result<glass_core::Frame> {
        self.inner.capture_frame_by(region, deadline)
    }

    fn capture_window_by(
        &mut self,
        id: glass_core::WindowId,
        region: Option<&glass_core::Region>,
        deadline: Deadline,
    ) -> glass_core::Result<glass_core::Frame> {
        self.inner.capture_window_by(id, region, deadline)
    }

    fn send_pointer_by(
        &mut self,
        event: &PointerEvent,
        deadline: Deadline,
    ) -> glass_core::Result<()> {
        let result = self.inner.send_pointer_by(event, deadline);
        if result.is_ok() {
            self.pointer_events.lock().unwrap().push(event.clone());
        }
        result
    }

    fn send_key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> glass_core::Result<()> {
        let result = self.inner.send_key_by(event, deadline);
        if result.is_ok() {
            self.key_events.lock().unwrap().push(event.clone());
        }
        result
    }

    fn window_by(
        &mut self,
        op: &glass_core::WindowOp,
        deadline: Deadline,
    ) -> glass_core::Result<glass_core::WindowGeometry> {
        self.inner.window_by(op, deadline)
    }

    fn list_windows_by(
        &mut self,
        deadline: Deadline,
    ) -> glass_core::Result<Vec<glass_core::WindowInfo>> {
        self.inner.list_windows_by(deadline)
    }

    fn select_window_by(
        &mut self,
        id: glass_core::WindowId,
        deadline: Deadline,
    ) -> glass_core::Result<glass_core::WindowGeometry> {
        self.inner.select_window_by(id, deadline)
    }

    fn drain_logs(&mut self) -> Vec<(glass_core::Stream, String)> {
        self.inner.drain_logs()
    }

    fn app_pid(&self) -> Option<u32> {
        self.inner.app_pid()
    }

    fn a11y_toggle_control_at_trailing_edge(&self) -> bool {
        self.inner.a11y_toggle_control_at_trailing_edge()
    }
}

type SemanticFixtureSession = (
    Glass,
    IosA11y,
    Arc<Mutex<Vec<PointerEvent>>>,
    Arc<Mutex<Vec<KeyEvent>>>,
);

fn semantic_fixture_session() -> SemanticFixtureSession {
    let registry = Box::new(SimulatorRegistry::new());
    let platform = IosPlatform::from_env(&registry)
        .expect("from_env: resolve/boot a Simulator and open the idb_companion input client");
    let session_reader = platform
        .accessibility()
        .expect("connect the session accessibility reader")
        .expect("companion is required for this on-box test");
    let independent_reader = platform
        .accessibility()
        .expect("connect the independent stale-target reader")
        .expect("companion is required for this on-box test");
    let pointer_events = Arc::new(Mutex::new(Vec::new()));
    let key_events = Arc::new(Mutex::new(Vec::new()));
    let mut backend = Some(Backend {
        platform: Box::new(CountingIosPlatform {
            inner: platform,
            pointer_events: Arc::clone(&pointer_events),
            key_events: Arc::clone(&key_events),
        }),
        accessibility: Some(Box::new(session_reader)),
    });
    let factory: glass_core::PlatformFactory = Box::new(move |_: &str| {
        let _keep_registry_alive = &registry;
        backend.take().ok_or_else(|| {
            glass_core::GlassError::Backend("iOS backend already constructed".into())
        })
    });
    let baselines = std::env::temp_dir().join(format!(
        "glass-ios-semantic-integration-{}",
        std::process::id()
    ));
    let glass = Glass::new(factory, "ios".into(), BaselineStore::new(baselines), 64);
    (glass, independent_reader, pointer_events, key_events)
}

fn fresh_until(glass: &mut Glass, attempts: usize, pred: impl Fn(&AxTree) -> bool) -> AxTree {
    let mut tree = glass.a11y_snapshot(None).expect("fresh semantic snapshot");
    for _ in 0..attempts {
        if pred(&tree) {
            return tree;
        }
        std::thread::sleep(Duration::from_millis(50));
        tree = glass.a11y_snapshot(None).expect("fresh semantic snapshot");
    }
    tree
}

fn check_verdict(
    checks: &[glass_core::ActionabilityCheck],
    name: ActionabilityCheckName,
) -> ActionabilityVerdict {
    checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("missing {name:?} actionability check: {checks:?}"))
        .verdict
}

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + idb_companion + a booted iOS \
            Simulator, and GLASS_IOS_APP pointing at the examples/ios-fixture .app"]
fn drive_fixture_snapshot_tap_and_type_end_to_end() {
    let app = std::env::var("GLASS_IOS_APP")
        .expect("GLASS_IOS_APP must be set to the examples/ios-fixture GlassFixture.app path");

    let spec = AppSpec {
        build: None,
        run: vec![app],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        a11y: true,
    };

    let reg = SimulatorRegistry::new();
    let mut platform = IosPlatform::from_env(&reg)
        .expect("from_env: resolve/boot a Simulator and open the idb_companion input client");
    let window = platform
        .start_app(&spec)
        .expect("start_app: install, launch, discover the point→pixel scale, report geometry");
    assert!(
        window.width > 0 && window.height > 0,
        "launched app geometry must be non-zero, got {window:?}"
    );

    let mut a11y = platform
        .accessibility()
        .expect("accessibility(): connect a second idb client to the same companion socket")
        .expect("companion present on this on-box run, so a reader is available");
    let ctx = AxContext {
        pids: vec![],
        window: window.clone(),
        window_handle: None,
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
        deadline: Deadline::UNBOUNDED,
    };

    // 1) Snapshot: the fixture's elements must appear, and the status starts at READY. The
    // READY text lives in the element's value (idb reports it in AXLabel, behind the id).
    let initial = snapshot_until(&mut a11y, &ctx, 10, |t| {
        named_value(t, "statusLabel").as_deref() == Some("READY")
    });
    let outline = initial.to_outline();
    println!("--- initial snapshot ---\n{outline}");
    assert!(
        outline.contains("tapButton"),
        "snapshot must contain tapButton:\n{outline}"
    );
    assert!(
        outline.contains("inputField"),
        "snapshot must contain inputField:\n{outline}"
    );
    assert_eq!(
        named_value(&initial, "statusLabel").as_deref(),
        Some("READY"),
        "status must start at READY"
    );

    // 2) Tap the button at the CENTER OF ITS SNAPSHOT BOUNDS (window pixels). If the point→
    // pixel scale chain is right, the injected touch lands on the button and flips the status
    // to TAPPED — the end-to-end proof that the tap reached the intended element.
    let (bx, by) = center_of(&initial, "tapButton", &window);
    println!("tapping tapButton at window-pixel ({bx},{by})");
    platform.send_pointer(&tap(bx, by)).expect("send tap");

    let after_tap = snapshot_until(&mut a11y, &ctx, 12, |t| {
        named_value(t, "statusLabel").as_deref() == Some("TAPPED")
    });
    println!("--- after tap ---\n{}", after_tap.to_outline());
    assert_eq!(
        named_value(&after_tap, "statusLabel").as_deref(),
        Some("TAPPED"),
        "the tap must flip statusLabel READY→TAPPED (proves it landed at the scaled coordinate)"
    );

    // 3) Focus the field with a tap, then type with the raw send_key path. The echo label
    // mirrors the field's text, so it is the ground-truth oracle for what was typed.
    let (fx, fy) = center_of(&after_tap, "inputField", &window);
    println!("focusing inputField at window-pixel ({fx},{fy})");
    platform.send_pointer(&tap(fx, fy)).expect("focus field");
    // Let the keyboard finish presenting before typing into the focused field.
    std::thread::sleep(Duration::from_millis(700));
    platform
        .send_key(&KeyEvent::Text("hello".into()))
        .expect("send_key type");
    let typed = snapshot_until(&mut a11y, &ctx, 12, |t| echo_is(t, "hello"));
    assert!(
        echo_is(&typed, "hello"),
        "send_key text must reach the focused field (echoLabel mirrors it); got {:?}",
        named_value(&typed, "echoLabel")
    );

    // 4) set_value replaces the field's contents: it re-verifies the target, taps to focus,
    // clears (select-all + delete), then types. Starting from "hello", it must yield exactly
    // "world" — a leftover like "helloworld" would mean the clear step did not fire, so this
    // is where the clear-then-type sequence is validated against the real Simulator.
    let for_target = a11y
        .snapshot(&ctx)
        .expect("snapshot for the set_value target");
    let field = find_named(&for_target.root, "inputField").expect("inputField present");
    let target = AxTarget {
        id: field.id,
        role: field.role,
        name: field.name.clone(),
        bounds: field.bounds,
        value: field.value.clone(),
    };
    a11y.set_value(&ctx, &target, "world")
        .expect("set_value: clear then type");
    let replaced = snapshot_until(&mut a11y, &ctx, 12, |t| echo_is(t, "world"));
    assert!(
        echo_is(&replaced, "world"),
        "set_value must clear \"hello\" and type \"world\"; got {:?} — a leftover like \
         \"helloworld\" means the clear step failed",
        named_value(&replaced, "echoLabel")
    );

    platform.stop_app().expect("stop_app");
}

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + idb_companion + a booted iOS \
            Simulator, and GLASS_IOS_APP pointing at the examples/ios-fixture .app"]
fn semantic_actions_refuse_unproven_ios_state_and_dispatch_once() {
    let app = std::env::var("GLASS_IOS_APP")
        .expect("GLASS_IOS_APP must be set to the examples/ios-fixture GlassFixture.app path");
    let (mut glass, mut independent_reader, pointer_events, key_events) =
        semantic_fixture_session();
    let window = glass
        .start(&semantic_fixture_spec(app))
        .expect("start semantic fixture through Glass");

    let initial = fresh_until(&mut glass, 20, |tree| {
        named_value(tree, "statusLabel").as_deref() == Some("READY")
            && find_named(&tree.root, "movingSemantic").is_some()
    });
    assert_eq!(
        named_value(&initial, "statusLabel").as_deref(),
        Some("READY")
    );
    let initial_moving = find_named(&initial.root, "movingSemantic")
        .and_then(|node| node.bounds)
        .expect("movingSemantic has initial bounds");

    let pointers_before_save = pointer_events.lock().unwrap().len();
    let save = glass
        .click_target(&pointer_click("semanticSave", AxRole::Button, 2_000))
        .expect("semantic save pointer action");
    assert_eq!(save.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        check_verdict(
            &save.actionability.checks,
            ActionabilityCheckName::NonOccluded
        ),
        ActionabilityVerdict::Unproven
    );
    assert_eq!(
        check_verdict(&save.actionability.checks, ActionabilityCheckName::Visible),
        ActionabilityVerdict::Unproven
    );
    assert_eq!(
        check_verdict(&save.actionability.checks, ActionabilityCheckName::InWindow),
        ActionabilityVerdict::Passed
    );
    assert_eq!(
        pointer_events.lock().unwrap().len(),
        pointers_before_save + 1,
        "semanticSave sends one pointer submission"
    );

    let mut observed_bounds = Vec::new();
    let motion_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let tree = glass
            .a11y_snapshot(None)
            .expect("motion observation snapshot");
        let bounds = find_named(&tree.root, "movingSemantic")
            .and_then(|node| node.bounds)
            .expect("movingSemantic stays published while moving");
        let save_counted = named_value(&tree, "statusLabel").as_deref() == Some("SAVED:1 MOVED:0");
        if observed_bounds.last().copied() != Some(bounds) {
            observed_bounds.push(bounds);
        }
        if observed_bounds.len() >= 2 && save_counted {
            break;
        }
        assert!(
            std::time::Instant::now() < motion_deadline,
            "movingSemantic never published two changing bounds with the exact save count: \
             {observed_bounds:?}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    println!("movingSemantic initial={initial_moving:?}, changing bounds={observed_bounds:?}");

    let pointers_before_moving = pointer_events.lock().unwrap().len();
    let moving = glass
        .click_target(&pointer_click("movingSemantic", AxRole::Button, 2_000))
        .expect("moving target waits for stable bounds then dispatches");
    assert_eq!(moving.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        check_verdict(&moving.actionability.checks, ActionabilityCheckName::Stable),
        ActionabilityVerdict::Passed
    );
    assert_eq!(
        pointer_events.lock().unwrap().len(),
        pointers_before_moving + 1,
        "movingSemantic sends one pointer submission after stability"
    );
    std::thread::sleep(Duration::from_millis(350));
    let after_moving = glass.a11y_snapshot(None).expect("quiet moving observation");
    assert_eq!(
        named_value(&after_moving, "statusLabel").as_deref(),
        Some("SAVED:1 MOVED:1"),
        "both semantic controls must remain at exactly one dispatch through the quiet window"
    );

    let pointers_before_refusals = pointer_events.lock().unwrap().len();
    let duplicate = glass
        .click_target(&pointer_click("duplicateSemantic", AxRole::Button, 0))
        .expect_err("duplicate selector must refuse without a tap");
    assert_eq!(duplicate.kind, SemanticActionFailureKind::AmbiguousTarget);
    assert_eq!(duplicate.action_dispatch, DispatchStatus::NotDispatched);
    let disabled = glass
        .click_target(&pointer_click("disabledSemantic", AxRole::Button, 0))
        .expect_err("known disabled selector must refuse without a tap");
    assert_eq!(disabled.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(disabled.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(
        check_verdict(
            &disabled.actionability.checks,
            ActionabilityCheckName::Enabled
        ),
        ActionabilityVerdict::Failed
    );
    std::thread::sleep(Duration::from_millis(350));
    let after_refusals = glass
        .a11y_snapshot(None)
        .expect("quiet refusal observation");
    assert_eq!(
        named_value(&after_refusals, "statusLabel").as_deref(),
        Some("SAVED:1 MOVED:1"),
        "duplicate and disabled refusals must deliver no fixture tap"
    );
    assert_eq!(
        pointer_events.lock().unwrap().len(),
        pointers_before_refusals,
        "duplicate and disabled refusals submit zero pointer events"
    );

    let stale_ctx = AxContext {
        pids: vec![],
        window: window.clone(),
        window_handle: None,
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
        deadline: Deadline::UNBOUNDED,
    };
    let stale_tree = independent_reader
        .snapshot(&stale_ctx)
        .expect("capture stale inputField identity before semantic write");
    let stale_field = find_named(&stale_tree.root, "inputField").expect("inputField present");
    let stale_target = AxTarget {
        id: stale_field.id,
        role: stale_field.role,
        name: stale_field.name.clone(),
        bounds: stale_field.bounds,
        value: stale_field.value.clone(),
    };

    let set_value = glass
        .set_value_target(
            &SetValueTargetParams {
                target: ActionTarget::Semantic(semantic_target("inputField", AxRole::TextField)),
                timeout_ms: Some(3_000),
                max_nodes: None,
            },
            "semantic-value",
        )
        .expect("semantic set-value resolves fresh and confirms its value");
    assert_eq!(set_value.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        set_value.action.confirmation,
        ConfirmationStatus::ValueConfirmed
    );
    let written = fresh_until(&mut glass, 12, |tree| {
        named_value(tree, "echoLabel").as_deref() == Some("semantic-value")
    });
    assert_eq!(
        named_value(&written, "echoLabel").as_deref(),
        Some("semantic-value")
    );

    let stale_error = independent_reader
        .set_value(&stale_ctx, &stale_target, "stale-write")
        .expect_err("stale value identity must refuse before dispatch");
    assert!(
        matches!(stale_error, glass_core::GlassError::AxElementChanged(_)),
        "{stale_error}"
    );
    std::thread::sleep(Duration::from_millis(350));
    let after_stale = glass
        .a11y_snapshot(None)
        .expect("quiet stale refusal observation");
    assert_eq!(
        named_value(&after_stale, "echoLabel").as_deref(),
        Some("semantic-value"),
        "stale identity refusal must not deliver text"
    );

    let pointers_before_type = pointer_events.lock().unwrap().len();
    let keys_before_type = key_events.lock().unwrap().len();
    let type_error = glass
        .type_target(
            &TypeTargetParams {
                target: semantic_target("inputField", AxRole::TextField),
                focus_mode: ActionMode::Pointer,
                timeout_ms: 500,
                max_nodes: None,
            },
            "forbidden-text",
        )
        .expect_err("idb cannot confirm focus, so targeted typing must stop after the focus tap");
    assert_eq!(type_error.kind, SemanticActionFailureKind::FocusUnconfirmed);
    assert_eq!(type_error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(
        type_error.focus,
        Some(MutationReport {
            method: ActionMethod::Pointer {
                native_fallback: None,
            },
            dispatch: DispatchStatus::Dispatched,
            confirmation: ConfirmationStatus::Unconfirmed,
        })
    );
    assert_eq!(
        pointer_events.lock().unwrap().len(),
        pointers_before_type + 1,
        "targeted type sends exactly one focus tap"
    );
    assert_eq!(
        key_events.lock().unwrap().len(),
        keys_before_type,
        "unconfirmed focus sends zero text key submissions"
    );
    std::thread::sleep(Duration::from_millis(350));
    let after_type = glass
        .a11y_snapshot(None)
        .expect("quiet targeted-type observation");
    assert_eq!(
        named_value(&after_type, "echoLabel").as_deref(),
        Some("semantic-value"),
        "unconfirmed focus must dispatch zero text to the field"
    );

    glass.stop().expect("stop semantic fixture");
}

/// The page's own elements, by the accessible name each carries on a platform that exposes web
/// content at all. Shared with every other platform's web reading — the page is one file.
const WEB_BUTTON: &str = "click me";
/// See [`WEB_BUTTON`]; the text input's name comes from its `<label for>`.
const WEB_INPUT: &str = "text input";
/// What the page's result paragraph reads after the button fires.
const WEB_CLICKED: &str = "clicked";
/// What `set_value` writes into the text input.
const WEB_TYPED: &str = "typed by glass";
/// What the keyboard control types into the same field — a second route to it.
const WEB_KEYED: &str = "keyed by glass";
/// The `WKWebView`'s own `accessibilityIdentifier`, so the tree can be asked whether the web
/// view itself arrived even when its content did not.
const WEB_VIEW_ID: &str = "the-web-view";

/// How long to let the page load before the first read. A `file://` page in a fresh web view is
/// quick, but "the tree is empty" and "the page had not painted" are the two readings this test
/// must never confuse, and the screenshot in `examples/ios-role-fixture/README.md` is what says
/// which one this is.
const WEB_LOAD_SETTLE: Duration = Duration::from_secs(3);

/// The first node carrying `text` as its whole name or value. Exact rather than a substring: the
/// page's result paragraph reads "not clicked" before the button fires, which contains
/// "clicked".
fn carries<'a>(tree: &'a AxTree, text: &str) -> Option<&'a AxNode> {
    fn walk<'a>(node: &'a AxNode, text: &str) -> Option<&'a AxNode> {
        if node.name.as_deref() == Some(text) || node.value.as_deref() == Some(text) {
            return Some(node);
        }
        node.children.iter().find_map(|c| walk(c, text))
    }
    walk(&tree.root, text)
}

/// How many `Document` nodes the tree holds — the web-content boundary, and what
/// `AxTree::document_guidance` discloses when one arrives with no children.
fn document_count(tree: &AxTree) -> usize {
    fn walk(node: &AxNode) -> usize {
        usize::from(node.role == AxRole::Document) + node.children.iter().map(walk).sum::<usize>()
    }
    walk(&tree.root)
}

/// `node`'s window-pixel click centre, or `None` when it has no on-screen geometry.
fn center_or_none(node: &AxNode, win: &glass_core::WindowGeometry) -> Option<(i32, i32)> {
    node.bounds?.clamped_center(win.width, win.height)
}

/// Everything the tree says about itself, so a run that reads nothing still says what it read.
fn report(label: &str, tree: &AxTree) {
    println!(
        "--- {label}: {} nodes, {} Document(s), complete={} ---",
        tree.count,
        document_count(tree),
        tree.is_complete()
    );
    for notice in [
        tree.truncation_notice(),
        tree.unreadable_notice(),
        tree.subject_notice(),
        tree.empty_guidance().map(str::to_string),
        tree.document_guidance(),
    ]
    .into_iter()
    .flatten()
    {
        println!("notice: {notice}");
    }
    println!("{}", tree.to_outline());
}

/// Read what a `WKWebView`'s page offers the driving path on iOS: whether the page's elements
/// are in the tree at all, and if they are, whether a tap on the button and a `set_value` on the
/// text input take.
///
/// A probe, not a mapping test. Only `start_app` and `snapshot` are asserted — both must FAIL the
/// test rather than return quietly, since a launch that never happened would otherwise print an
/// empty tree that reads exactly like "the engine published nothing". Everything after that is
/// printed: an element the tree does not carry is a finding, and the test says so instead of
/// panicking on it.
#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + idb_companion + a booted iOS \
            Simulator, and GLASS_IOS_ROLE_FIXTURE pointing at the examples/ios-role-fixture \
            .app"]
fn web_fixture_button_and_field_respond() {
    let app = std::env::var("GLASS_IOS_ROLE_FIXTURE").expect(
        "GLASS_IOS_ROLE_FIXTURE must be set to the examples/ios-role-fixture RoleFixture.app path",
    );

    // The web tab is chosen at launch: the tab bar's items are not accessibility elements and a
    // synthetic tap on one does not switch tabs (see the fixture's README).
    let spec = AppSpec {
        build: None,
        run: vec![app, "--tab".into(), "web".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        a11y: true,
    };

    let reg = SimulatorRegistry::new();
    let mut platform = IosPlatform::from_env(&reg)
        .expect("from_env: resolve/boot a Simulator and open the idb_companion input client");
    let window = platform
        .start_app(&spec)
        .expect("start_app: install, launch, discover the point→pixel scale, report geometry");

    let mut a11y = platform
        .accessibility()
        .expect("accessibility(): connect a second idb client to the same companion socket")
        .expect("companion present on this on-box run, so a reader is available");
    let ctx = AxContext {
        pids: vec![],
        window: window.clone(),
        window_handle: None,
        a11y_bus_addr: None,
        // The node cap lifted: a page's tree is the thing being read, and a truncated walk would
        // make an absent element indistinguishable from an unreached one.
        limits: WalkLimits::from_max_nodes(Some(0)),
        deadline: Deadline::UNBOUNDED,
    };

    std::thread::sleep(WEB_LOAD_SETTLE);
    let mut initial = snapshot_until(&mut a11y, &ctx, 20, |t| carries(t, WEB_BUTTON).is_some());
    initial.assign_ids();
    report("initial", &initial);
    println!(
        "web view element {WEB_VIEW_ID:?} present: {}",
        find_named(&initial.root, WEB_VIEW_ID).is_some()
    );

    match find_named(&initial.root, WEB_BUTTON) {
        Some(button) => match center_or_none(button, &window) {
            Some((bx, by)) => {
                println!("tapping {WEB_BUTTON:?} at window-pixel ({bx},{by})");
                platform.send_pointer(&tap(bx, by)).expect("send tap");
                let mut after =
                    snapshot_until(&mut a11y, &ctx, 12, |t| carries(t, WEB_CLICKED).is_some());
                after.assign_ids();
                println!(
                    "after the tap, the result paragraph reads {WEB_CLICKED:?}: {}",
                    carries(&after, WEB_CLICKED).is_some()
                );
                report("after the tap", &after);
            }
            None => println!("{WEB_BUTTON:?} is in the tree with no on-screen bounds: no tap"),
        },
        None => println!(
            "no node named {WEB_BUTTON:?} — the page's elements are not in the tree, so there is \
             nothing to address: no tap reading, and no set_value reading either"
        ),
    }

    let mut for_target = a11y
        .snapshot(&ctx)
        .expect("snapshot for the set_value target");
    for_target.assign_ids();
    match find_named(&for_target.root, WEB_INPUT) {
        Some(field) => {
            let target = AxTarget {
                id: field.id,
                role: field.role,
                name: field.name.clone(),
                bounds: field.bounds,
                value: field.value.clone(),
            };
            println!(
                "set_value({WEB_INPUT:?}): {:?}",
                a11y.set_value(&ctx, &target, WEB_TYPED)
            );
            let mut after =
                snapshot_until(&mut a11y, &ctx, 12, |t| carries(t, WEB_TYPED).is_some());
            after.assign_ids();
            println!(
                "text input value after set_value: {:?}",
                find_named(&after.root, WEB_INPUT).and_then(|n| n.value.clone())
            );

            // The control. An empty read-back has two causes — the write never landed, or this
            // reader never reports a web input's text — and only text that reached the field by
            // another route tells them apart.
            if let Some(field) = find_named(&after.root, WEB_INPUT)
                && let Some((fx, fy)) = center_or_none(field, &window)
            {
                platform
                    .send_pointer(&tap(fx, fy))
                    .expect("focus the field");
                std::thread::sleep(Duration::from_millis(700));
                platform
                    .send_key(&KeyEvent::Text(WEB_KEYED.into()))
                    .expect("send_key type");
                let mut keyed =
                    snapshot_until(&mut a11y, &ctx, 12, |t| carries(t, WEB_KEYED).is_some());
                keyed.assign_ids();
                println!(
                    "control — tap then type → text input value: {:?}",
                    find_named(&keyed.root, WEB_INPUT).and_then(|n| n.value.clone())
                );
            }
        }
        None => println!("no node named {WEB_INPUT:?} — no set_value reading"),
    }

    platform.stop_app().expect("stop_app");
}
