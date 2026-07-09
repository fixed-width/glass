//! End-to-end drive test for the iOS Simulator backend's input + accessibility.
//!
//! Launches a real fixture app on a booted Simulator and drives it through the public
//! `glass_core` seams — `Platform` for input and `Accessibility` for the tree — exactly the
//! path `glass_click`/`glass_type`/`glass_a11y_snapshot`/`glass_set_value` exercise over MCP.
//! It proves the whole chain end to end: a tap issued at a snapshot element's window-pixel
//! center lands on that element (the READY→TAPPED flip), and typed text — both raw
//! `send_key` and the `set_value` clear-then-type sequence — reaches the field.
//!
//! `#[ignore]`d so a plain `cargo test` (Linux dev host, CI) skips it: the backend needs
//! `xcrun simctl` + `idb_companion` (macOS + Xcode only), a booted Simulator, and the
//! GlassFixture app. Run explicitly on such a host:
//!
//! ```sh
//! GLASS_IOS_APP=/path/to/GlassFixture.app \
//!   cargo test -p glass-ios --test drive_integration -- --ignored --nocapture
//! ```
//!
//! `GLASS_IOS_APP` must be a `.app` bundle path so `start_app` installs it itself.
//! `GLASS_IOS_UDID` / `GLASS_IOS_DEVICE` / `GLASS_IDB_COMPANION` select the Simulator and the
//! companion binary the same way they do for `glass-mcp`; see `docs/how-to/setup-ios.md`.
//!
//! The fixture exposes four accessibility elements (by `AXUniqueId`): `statusLabel` (shows
//! READY, flips to TAPPED when the button is tapped), `tapButton`, `inputField`, and
//! `echoLabel` (mirrors the field's text, or `(empty)`).

use std::time::Duration;

use glass_core::accessibility::{Accessibility, AxContext, AxNode, AxTarget, AxTree};
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

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + idb_companion + a booted iOS \
            Simulator, and GLASS_IOS_APP pointing at the GlassFixture .app"]
fn drive_fixture_snapshot_tap_and_type_end_to_end() {
    let app = std::env::var("GLASS_IOS_APP")
        .expect("GLASS_IOS_APP must be set to the GlassFixture .app path");

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
