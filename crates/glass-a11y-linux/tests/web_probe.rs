//! Web-content PROBE for the Linux backends — prints what a browser publishes over AT-SPI
//! under glass's private bus, with and without the candidate enable levers. `#[ignore]`d:
//! it needs a browser installed and the same prerequisites as `scripts/test-a11y.sh`.
//!
//! ```sh
//! GLASS_WEB_PROBE_BROWSERS=firefox,brave-browser \
//!   cargo test -p glass-a11y-linux --test web_probe -- --ignored --nocapture
//! ```
//!
//! `GLASS_WEB_PROBE_LEVER` picks a candidate enable lever, carried by the launch spec alone so
//! no product code is involved: `1` (or `both`) puts `GNOME_ACCESSIBILITY=1` and
//! `ACCESSIBILITY_ENABLED=1` in the environment, `gnome` and `enabled` set one each (to tell
//! which of the two a browser reads), and `flag` passes the Chromium-family
//! `--force-renderer-accessibility` switch instead. Unset is the baseline reading.
//!
//! With `GLASS_WEB_PROBE_BROWSERS` unset both tests print a skip line and return, so an
//! `--include-ignored` run never needs a browser.
//!
//! A probe, not a mapping test: it prints evidence and does not assert what a browser ought to
//! publish. After `start` it reports failures instead of panicking, so `stop` always runs and
//! no browser is left behind.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, AxNode, AxRole, AxTree, Backend, BaselineStore, Glass, GlassError, KeyEvent,
    PlatformFactory, SandboxLevel, WindowHint, role_histogram,
};

const BROWSERS_VAR: &str = "GLASS_WEB_PROBE_BROWSERS";
const LEVER_VAR: &str = "GLASS_WEB_PROBE_LEVER";

/// How long to keep re-reading the tree for the page's own elements before calling the content
/// missing. The reading this bounds is "time to content", so it is deliberately generous.
const SETTLE: Duration = Duration::from_secs(20);

/// Window-discovery budget. A cold browser opening a fresh profile under a software renderer
/// maps its window far later than the GTK fixture does.
const LAUNCH_TIMEOUT_MS: u64 = 30_000;

/// The fixture button's accessible name — the page content the probe waits for, then clicks.
const BUTTON: &str = "click me";
/// The fixture text input's accessible name, from its `<label for>`.
const INPUT: &str = "text input";
/// What the page's result paragraph reads before the button fires, and after.
const NOT_CLICKED: &str = "not clicked";
/// See [`NOT_CLICKED`].
const CLICKED: &str = "clicked";
/// What `set_value` writes into the text input.
const TYPED: &str = "typed by glass";
/// What the keyboard control types into the text input — a second route to the same field.
const KEYED: &str = "keyed by glass";

fn page_url() -> String {
    format!(
        "file://{}/../../examples/web-role-fixture/index.html",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Which candidate enable lever the launch carries: environment variables, or the
/// Chromium-family command-line switch.
#[derive(Clone, Copy, Debug)]
enum Lever {
    None,
    Gnome,
    Enabled,
    Both,
    RendererFlag,
}

impl Lever {
    fn from_env() -> Self {
        match std::env::var(LEVER_VAR).unwrap_or_default().as_str() {
            "1" | "both" => Lever::Both,
            "gnome" => Lever::Gnome,
            "enabled" => Lever::Enabled,
            "flag" => Lever::RendererFlag,
            _ => Lever::None,
        }
    }

    fn vars(self) -> Vec<(String, String)> {
        let gnome = ("GNOME_ACCESSIBILITY".to_string(), "1".to_string());
        let enabled = ("ACCESSIBILITY_ENABLED".to_string(), "1".to_string());
        match self {
            Lever::None | Lever::RendererFlag => Vec::new(),
            Lever::Gnome => vec![gnome],
            Lever::Enabled => vec![enabled],
            Lever::Both => vec![gnome, enabled],
        }
    }

    /// The Chromium-family switch that turns the renderer's accessibility on without waiting
    /// for a client to be detected. Nothing for the other levers, and nothing for Firefox —
    /// Gecko has no equivalent switch.
    fn chromium_args(self) -> Vec<String> {
        match self {
            Lever::RendererFlag => vec!["--force-renderer-accessibility".to_string()],
            _ => Vec::new(),
        }
    }
}

fn glass_for(backend: &str) -> Glass {
    let name = backend.to_string();
    let factory: PlatformFactory = Box::new(move |_| {
        let platform: Box<dyn glass_core::Platform + Send> = match name.as_str() {
            "x11" => Box::new(glass_x11::X11Platform::from_env()?),
            _ => Box::new(glass_wayland::WaylandPlatform::new()?),
        };
        Ok(Backend {
            platform,
            accessibility: Some(Box::new(glass_a11y_linux::LinuxA11y::new())),
        })
    });
    let dir = tempfile::tempdir().expect("baseline dir");
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    Glass::new(factory, backend.into(), BaselineStore::new(root), 100)
}

/// A fresh, isolated profile per launch: no first-run prompts, no session restore, and no
/// already-running instance adopting the URL instead of starting a process glass owns.
fn browser_spec(browser: &str, profile: &Path, lever: Lever) -> AppSpec {
    let run = if browser.contains("firefox") {
        vec![
            browser.to_string(),
            "--no-remote".into(),
            "--new-instance".into(),
            "--profile".into(),
            profile.display().to_string(),
            page_url(),
        ]
    } else {
        let mut run = vec![
            browser.to_string(),
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-gpu".into(),
        ];
        run.extend(lever.chromium_args());
        run.push(page_url());
        run
    };
    let mut env = vec![("LIBGL_ALWAYS_SOFTWARE".to_string(), "1".to_string())];
    env.extend(lever.vars());
    AppSpec {
        build: None,
        run,
        cwd: None,
        env,
        // The title hint is a fallback only: window discovery matches the launched process
        // tree's `_NET_WM_PID` first, and a browser appends its own name to the page title.
        window_hint: Some(WindowHint {
            title: Some("Glass web fixture".into()),
            class: None,
        }),
        timeout_ms: LAUNCH_TIMEOUT_MS,
        sandbox: SandboxLevel::Off,
        a11y: true,
    }
}

fn find<'a>(tree: &'a AxTree, pred: &dyn Fn(&AxNode) -> bool) -> Option<&'a AxNode> {
    fn walk<'a>(node: &'a AxNode, pred: &dyn Fn(&AxNode) -> bool) -> Option<&'a AxNode> {
        if pred(node) {
            return Some(node);
        }
        node.children.iter().find_map(|c| walk(c, pred))
    }
    walk(&tree.root, pred)
}

/// The first node whose accessible name is exactly `name`.
fn named<'a>(tree: &'a AxTree, name: &str) -> Option<&'a AxNode> {
    find(tree, &|n| n.name.as_deref() == Some(name))
}

/// The fixture's text input, not its `<label for>`: both carry the same accessible name, and
/// the label comes first in pre-order, so `named` alone writes to the wrong element.
fn text_input(tree: &AxTree) -> Option<&AxNode> {
    find(tree, &|n| {
        n.name.as_deref() == Some(INPUT) && n.states.editable
    })
}

/// The fixture's result paragraph, found by the text it carries. The text of a `<p>` is a
/// node's `value` (the reader reads the AT-SPI `Text` interface for text-bearing roles), not
/// its name, and the outline render does not print values — so this is the only place the
/// before/after of the click is visible.
fn valued<'a>(tree: &'a AxTree, value: &str) -> Option<&'a AxNode> {
    find(tree, &|n| n.value.as_deref() == Some(value))
}

/// Every editable node in the tree, in pre-order.
fn editable(tree: &AxTree) -> Vec<&AxNode> {
    fn walk<'a>(node: &'a AxNode, out: &mut Vec<&'a AxNode>) {
        if node.states.editable {
            out.push(node);
        }
        for child in &node.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(&tree.root, &mut out);
    out
}

/// Every `Document` in the tree, in pre-order — the web-content boundary this probe reads.
fn documents(tree: &AxTree) -> Vec<&AxNode> {
    fn walk<'a>(node: &'a AxNode, out: &mut Vec<&'a AxNode>) {
        if node.role == AxRole::Document {
            out.push(node);
        }
        for child in &node.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(&tree.root, &mut out);
    out
}

/// Re-snapshot until the page's own elements arrive or `SETTLE` elapses. Returns the last tree
/// read, how long it took (the settle reading), and whether the content arrived.
///
/// Arrival requires the button to carry bounds, not merely to exist: the engine publishes the
/// node before it has laid the page out, and a click on a node with no bounds is refused.
///
/// Every error is retried until the deadline, not just `AccessibilityNotReady`: a browser
/// re-execs during startup, and the bus error its first process leaves behind resolves itself
/// once the second one registers. Each distinct error is printed once — a probe that stops at
/// the first one reads "nothing published" for a browser that had not started yet.
fn snapshot_until_page(glass: &mut Glass) -> (Option<AxTree>, Duration, bool) {
    let start = Instant::now();
    let mut last = None;
    let mut reported = Vec::new();
    loop {
        match glass.a11y_snapshot(Some(0)) {
            Ok(tree) => {
                let arrived = named(&tree, BUTTON).is_some_and(|n| n.bounds.is_some());
                last = Some(tree);
                if arrived {
                    return (last, start.elapsed(), true);
                }
            }
            Err(GlassError::AccessibilityNotReady(_)) => {}
            Err(e) => {
                let text = e.to_string();
                if !reported.contains(&text) {
                    println!("snapshot error at {:?}: {text}", start.elapsed());
                    reported.push(text);
                }
            }
        }
        if start.elapsed() > SETTLE {
            return (last, start.elapsed(), false);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn version_of(browser: &str) -> String {
    match Command::new(browser).arg("--version").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(e) => format!("(unreadable: {e})"),
    }
}

fn report_tree(tree: &AxTree) {
    for doc in documents(tree) {
        println!(
            "document: #{} raw_role={:?} name={:?} children={} bounds={:?}",
            doc.id.0,
            doc.raw_role,
            doc.name,
            doc.children.len(),
            doc.bounds
        );
    }
    println!("role histogram (role, count, native token):");
    for tally in role_histogram(tree) {
        println!("  {:?} {:>4}  {}", tally.role, tally.count, tally.raw_role);
    }
    for notice in [
        tree.truncation_notice(),
        tree.unreadable_notice(),
        tree.subject_notice(),
        tree.empty_guidance().map(str::to_string),
    ]
    .into_iter()
    .flatten()
    {
        println!("notice: {notice}");
    }
    println!("outline:\n{}", tree.to_outline());
}

/// How many times to re-snapshot-and-click. A browser's tree keeps changing while its other
/// tabs load, and an id whose node has moved since the snapshot is rejected as changed —
/// retrying reads the button's clickability rather than the retry's luck.
const CLICK_ATTEMPTS: usize = 4;

/// The actuation readings. Each step re-snapshots first: `click_element` and `set_value` resolve
/// ids against the session's most recent tree, so an id from an older one addresses nothing.
fn exercise(glass: &mut Glass) {
    for attempt in 1..=CLICK_ATTEMPTS {
        let before = match glass.a11y_snapshot(Some(0)) {
            Ok(tree) => tree,
            Err(e) => {
                println!("snapshot before the click failed: {e} — no click reading");
                return;
            }
        };
        let Some(button) = named(&before, BUTTON) else {
            println!("no node named {BUTTON:?} — no click reading");
            return;
        };
        if attempt == 1 {
            println!(
                "button: #{} role={:?} raw_role={} bounds={:?}",
                button.id.0, button.role, button.raw_role, button.bounds
            );
            println!(
                "result paragraph before the click: {:?}",
                valued(&before, NOT_CLICKED).map(|n| (n.id.0, n.role, n.raw_role.clone()))
            );
        }
        match glass.click_element(button.id) {
            Ok(method) => {
                std::thread::sleep(Duration::from_millis(500));
                match glass.a11y_snapshot(Some(0)) {
                    Ok(after) => println!(
                        "click_element (attempt {attempt}): {method:?} → result paragraph reads \
                         {CLICKED:?}: {}",
                        valued(&after, CLICKED).is_some()
                    ),
                    Err(e) => println!("click_element: {method:?} → re-snapshot failed: {e}"),
                }
                break;
            }
            Err(e) => {
                println!("click_element (attempt {attempt}) failed: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    let after = match glass.a11y_snapshot(Some(0)) {
        Ok(tree) => tree,
        Err(e) => {
            println!("snapshot before set_value failed: {e} — no set_value reading");
            return;
        }
    };
    let Some(field) = text_input(&after) else {
        println!("no editable node named {INPUT:?} — no set_value reading");
        return;
    };
    println!(
        "text input: #{} role={:?} raw_role={} value={:?}",
        field.id.0, field.role, field.raw_role, field.value
    );
    let set = glass.set_value(field.id, TYPED);
    std::thread::sleep(Duration::from_millis(500));
    let after = match glass.a11y_snapshot(Some(0)) {
        Ok(tree) => tree,
        Err(e) => {
            println!("set_value: {set:?} → re-snapshot failed: {e}");
            return;
        }
    };
    println!(
        "set_value: {set:?} → text input value={:?}",
        text_input(&after).and_then(|n| n.value.clone())
    );

    // The control for the line above. An empty readback has two causes — the write never
    // landed, or this engine never reports a web input's text over AT-SPI — and only text
    // that reached the field by another route tells them apart. Typed through the pointer
    // and keyboard, which do not touch the accessibility write path.
    if let Some(field) = text_input(&after) {
        let focus = glass.click_element(field.id);
        let keyed = glass.key(&KeyEvent::Text(KEYED.to_string()));
        std::thread::sleep(Duration::from_millis(500));
        match glass.a11y_snapshot(Some(0)) {
            Ok(after) => println!(
                "control — click {focus:?} then key {keyed:?} → text input value={:?}",
                text_input(&after).and_then(|n| n.value.clone())
            ),
            Err(e) => println!("control — re-snapshot failed: {e}"),
        }
    }

    match glass.a11y_snapshot(Some(0)) {
        Ok(after) => {
            println!("every editable node at the end:");
            for node in editable(&after) {
                println!(
                    "  #{} role={:?} raw_role={} name={:?} value={:?}",
                    node.id.0, node.role, node.raw_role, node.name, node.value
                );
            }
            println!("--- tree after actuation ---");
            report_tree(&after);
        }
        Err(e) => println!("final snapshot failed: {e}"),
    }
}

fn probe(backend: &str, browser: &str, lever: Lever) {
    println!("=== {backend} / {browser} / lever={lever:?} ===");
    println!("engine: {}", version_of(browser));

    let profile = tempfile::tempdir().expect("profile dir");
    let spec = browser_spec(browser, profile.path(), lever);
    println!("run: {:?}", spec.run);
    println!("env: {:?}", spec.env);

    let mut glass = glass_for(backend);
    let started = Instant::now();
    if let Err(e) = glass.start(&spec) {
        println!("start failed after {:?}: {e}", started.elapsed());
        return;
    }
    println!("window mapped after {:?}", started.elapsed());

    let (tree, settle, arrived) = snapshot_until_page(&mut glass);
    println!("page content arrived: {arrived} after {settle:?}");
    match tree {
        Some(tree) => {
            report_tree(&tree);
            if arrived {
                exercise(&mut glass);
            } else if let Some(hint) = tree.document_guidance() {
                println!("disclosure rendered:\n{hint}");
            } else {
                println!("NO DOCUMENT AND NO DISCLOSURE — the blind spot");
            }
        }
        None => println!("no tree at all — nothing was published within {SETTLE:?}"),
    }
    println!("stop: {:?}", glass.stop());
}

fn run_probes(backend: &str) {
    let Ok(list) = std::env::var(BROWSERS_VAR) else {
        println!("skipped: set {BROWSERS_VAR}=firefox,brave-browser");
        return;
    };
    let lever = Lever::from_env();
    for browser in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        probe(backend, browser, lever);
    }
}

#[test]
#[ignore = "needs a browser and the a11y prerequisites; see the module doc"]
fn x11_browsers() {
    run_probes("x11");
}

#[test]
#[ignore = "needs sway, a browser and the a11y prerequisites; see the module doc"]
fn wayland_browsers() {
    run_probes("wayland");
}
