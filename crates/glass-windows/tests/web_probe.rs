//! Web-content PROBE for the Windows backend — what a browser publishes over UI Automation for
//! the shared web fixture, and what `FrameworkId` a `Document` control type carries there versus
//! in a stock text editor. `#[ignore]`d and `#![cfg(windows)]` like `tests/onbox.rs`: it needs the
//! interactive desktop session, so it runs through the harness.
//!
//! ```sh
//! GLASS_WIN_HOST=user@host GLASS_WIN_REPO=C:/path/to/glass \
//!   ./scripts/test-windows.sh --tests web_probe
//! ```
//!
//! The harness's session-1 bounce forwards no environment, so browsers are located by their
//! per-machine install paths and the fixture path derives from `CARGO_MANIFEST_DIR`. A browser
//! that isn't installed prints a skip line.
//!
//! A probe, not a mapping test: it prints evidence and does not assert what an engine ought to
//! publish. The one assertion is teardown — nothing it launched may survive `stop`.
#![cfg(windows)]

use std::path::Path;
use std::time::{Duration, Instant};

use glass_a11y_windows::WindowsA11y;
use glass_core::{
    AppSpec, AxNode, AxRole, AxTree, Backend, BaselineStore, Glass, GlassError, KeyEvent,
    PlatformFactory, SandboxLevel, WindowHint, WindowOp, role_histogram,
};
use glass_windows::WindowsPlatform;
use uiautomation::controls::ControlType;
use uiautomation::types::Handle;
use uiautomation::{UIAutomation, UIElement};

/// The headline "time to content" bound: how long the probe re-reads the tree for the page's own
/// elements before calling the content missing.
const SETTLE: Duration = Duration::from_secs(8);
/// A second read taken only when [`SETTLE`] expired, so a slow arrival is told apart from no
/// arrival at all — a missing mapping must not be reported as missing content.
const EXTENDED_SETTLE: Duration = Duration::from_secs(20);
/// Window-discovery budget. A cold browser opening a fresh profile maps its window far later
/// than a native fixture does.
const LAUNCH_TIMEOUT_MS: u64 = 30_000;
/// Depth of the `Document` search, per the UIA reading recipe.
const DOC_DEPTH: u32 = 30;

/// The fixture button's accessible name — the page content the probe waits for, then clicks.
const BUTTON: &str = "click me";
/// The fixture text input's accessible name, from its `<label for>`.
const INPUT: &str = "text input";
/// The fixture's `<h1>`, and the page `<title>`.
const HEADING: &str = "Glass web fixture";
/// What the page's result paragraph reads after the button fires.
const CLICKED: &str = "clicked";
/// What `set_value` writes into the text input.
const TYPED: &str = "typed by glass";
/// What the keyboard control types into the text input — a second route to the same field.
const KEYED: &str = "keyed by glass";
/// What the Notepad leg types, so its `Document` has content to expose.
const NOTEPAD_LINE: &str = "glass web probe";

/// Each engine's per-machine install path, relative to a Program Files root, with the label the
/// findings table uses. Gecko is listed so a box that has it reads it.
const BROWSERS: [(&str, &str); 3] = [
    (
        "Edge (Chromium/WebView2)",
        r"Microsoft\Edge\Application\msedge.exe",
    ),
    (
        "Brave (Chromium)",
        r"BraveSoftware\Brave-Browser\Application\brave.exe",
    ),
    ("Firefox (Gecko)", r"Mozilla Firefox\firefox.exe"),
];

/// One engine's headline readings, for the aggregate line.
struct Reading {
    engine: &'static str,
    installed: bool,
    arrived: bool,
    clicked: bool,
    value_took: bool,
    frameworks: Vec<String>,
}

impl Reading {
    fn absent(engine: &'static str) -> Reading {
        Reading {
            engine,
            installed: false,
            arrived: false,
            clicked: false,
            value_took: false,
            frameworks: Vec::new(),
        }
    }
}

/// A `Glass` session wired to the real Windows backend + UIA reader, so the probe reads through
/// the production `click_element`/`set_value` orchestration rather than the reader alone. Same
/// shape as `tests/onbox.rs`'s `glass_windows_with_a11y`; the baseline dir is leaked deliberately,
/// this being a short-lived on-box test process.
fn glass_windows_with_a11y() -> Glass {
    let factory: PlatformFactory = Box::new(|_backend| {
        Ok(Backend {
            platform: Box::new(WindowsPlatform::new()?),
            accessibility: Some(Box::new(WindowsA11y::new())),
        })
    });
    let dir = tempfile::tempdir().expect("tempdir for baseline store");
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    Glass::new(factory, "windows".into(), BaselineStore::new(root), 100)
}

/// The shared web fixture as a `file://` URL, derived from this crate's manifest dir so it points
/// at whatever checkout the box built. Backslashes are swapped for forward slashes: a browser
/// takes the URL form, not the Windows path form.
fn page_url() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above crates/glass-windows")
        .join("examples")
        .join("web-role-fixture")
        .join("index.html");
    format!("file:///{}", path.display().to_string().replace('\\', "/"))
}

/// Locate an install under either Program Files root. The roots fall back to their standard
/// paths so the lookup holds even in a session that carries no environment.
fn locate(relative: &str) -> Option<String> {
    let roots = [
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into()),
        std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()),
    ];
    roots.into_iter().find_map(|base| {
        let candidate = format!("{base}\\{relative}");
        Path::new(&candidate).exists().then_some(candidate)
    })
}

/// A fresh, isolated profile per launch: no first-run prompts, no session restore, and no
/// already-running instance adopting the URL instead of starting a process glass owns. The
/// profile path carries `marker`, which is also how the teardown check finds our processes.
fn browser_spec(exe: &str, profile: &str) -> AppSpec {
    let run = if exe.to_ascii_lowercase().contains("firefox") {
        vec![
            exe.to_string(),
            "--no-remote".into(),
            "--new-instance".into(),
            "--profile".into(),
            profile.to_string(),
            page_url(),
        ]
    } else {
        vec![
            exe.to_string(),
            format!("--user-data-dir={profile}"),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            page_url(),
        ]
    };
    AppSpec {
        build: None,
        run,
        cwd: None,
        env: vec![],
        // A fallback only: window discovery matches the launched process tree first, and a
        // browser appends its own name to the page title.
        window_hint: Some(WindowHint {
            title: Some(HEADING.into()),
            class: None,
        }),
        timeout_ms: LAUNCH_TIMEOUT_MS,
        sandbox: SandboxLevel::Off,
        a11y: true,
    }
}

/// Notepad, for the text-editor `Document` reading. No `window_hint`: its own process (or the
/// descendant its launcher hands the UI to) is found by pid-set membership.
fn notepad_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["notepad.exe".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 15_000,
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

fn collect<'a>(tree: &'a AxTree, pred: &dyn Fn(&AxNode) -> bool) -> Vec<&'a AxNode> {
    fn walk<'a>(node: &'a AxNode, pred: &dyn Fn(&AxNode) -> bool, out: &mut Vec<&'a AxNode>) {
        if pred(node) {
            out.push(node);
        }
        for child in &node.children {
            walk(child, pred, out);
        }
    }
    let mut out = Vec::new();
    walk(&tree.root, pred, &mut out);
    out
}

/// The first node whose accessible name is exactly `name`.
fn named<'a>(tree: &'a AxTree, name: &str) -> Option<&'a AxNode> {
    find(tree, &|n| n.name.as_deref() == Some(name))
}

/// The fixture's text input, not its `<label for>`: both carry the same accessible name and the
/// label comes first in pre-order, so `named` alone writes to the wrong element.
fn text_input(tree: &AxTree) -> Option<&AxNode> {
    find(tree, &|n| {
        n.name.as_deref() == Some(INPUT) && n.states.editable
    })
}

/// A node carrying `text` in either its name or its value. The result paragraph's text reaches
/// `name` on an API that names a text element by its content and `value` on one that exposes a
/// text interface — looking in only one field would read a working click as a failed one.
fn carrying<'a>(tree: &'a AxTree, text: &str) -> Option<&'a AxNode> {
    find(tree, &|n| {
        n.name.as_deref() == Some(text) || n.value.as_deref() == Some(text)
    })
}

/// Every node the reader put a `Document` control type on, whatever role it mapped to — the
/// web-content boundary this probe reads. Matched on `raw_role` as well as `role` because the
/// two need not agree, and which one holds the token is part of the reading.
fn documents(tree: &AxTree) -> Vec<&AxNode> {
    collect(tree, &|n| {
        n.role == AxRole::Document || n.raw_role == "Document"
    })
}

/// Re-snapshot until the page's own elements arrive or `budget` elapses. Returns the last tree
/// read, how long it took, and whether the content arrived.
///
/// Arrival requires the fixture button to carry bounds, not merely to exist: an engine publishes
/// the node before it has laid the page out, and a click on a node with no bounds is refused.
/// The button, not the heading, is the trigger — a heading depends on a role mapping this
/// backend may not have, and content that arrived must not read as content that did not.
///
/// Every error is retried until the deadline: a browser re-execs during startup and the reader
/// has nothing to bind to until the second process owns the window. Each distinct error prints
/// once.
fn snapshot_until_page(glass: &mut Glass, budget: Duration) -> (Option<AxTree>, Duration, bool) {
    let start = Instant::now();
    let mut last = None;
    let mut reported: Vec<String> = Vec::new();
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
        if start.elapsed() > budget {
            return (last, start.elapsed(), false);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn report_tree(tree: &AxTree) {
    for doc in documents(tree) {
        println!(
            "document: #{} role={:?} raw_role={:?} name={:?} children={} bounds={:?}",
            doc.id.0,
            doc.role,
            doc.raw_role,
            doc.name,
            doc.children.len(),
            doc.bounds
        );
    }
    println!(
        "heading {HEADING:?} present: {}",
        find(tree, &|n| n.role == AxRole::Heading
            && n.name.as_deref() == Some(HEADING))
        .is_some()
    );
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

/// How many times to re-snapshot-and-click. A browser's tree keeps changing while the page
/// settles, and an id whose node has moved since the snapshot is rejected as changed.
const CLICK_ATTEMPTS: usize = 4;

/// The actuation readings. Each step re-snapshots first: `click_element` and `set_value` resolve
/// ids against the session's most recent tree, so an id from an older one addresses nothing.
/// Returns whether the click landed and whether the written value read back.
fn exercise(glass: &mut Glass) -> (bool, bool) {
    let mut clicked = false;
    for attempt in 1..=CLICK_ATTEMPTS {
        let before = match glass.a11y_snapshot(Some(0)) {
            Ok(tree) => tree,
            Err(e) => {
                println!("snapshot before the click failed: {e} — no click reading");
                return (false, false);
            }
        };
        let Some(button) = named(&before, BUTTON) else {
            println!("no node named {BUTTON:?} — no click reading");
            return (false, false);
        };
        if attempt == 1 {
            println!(
                "button: #{} role={:?} raw_role={} bounds={:?}",
                button.id.0, button.role, button.raw_role, button.bounds
            );
        }
        match glass.click_element(button.id) {
            Ok(method) => {
                std::thread::sleep(Duration::from_millis(500));
                match glass.a11y_snapshot(Some(0)) {
                    Ok(after) => {
                        clicked = carrying(&after, CLICKED).is_some();
                        println!(
                            "click_element (attempt {attempt}): {method:?} → result paragraph \
                             reads {CLICKED:?}: {clicked}"
                        );
                    }
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
            return (clicked, false);
        }
    };
    let Some(field) = text_input(&after) else {
        println!("no editable node named {INPUT:?} — no set_value reading");
        return (clicked, false);
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
            return (clicked, false);
        }
    };
    let read_back = text_input(&after).and_then(|n| n.value.clone());
    let value_took = read_back.as_deref() == Some(TYPED);
    println!("set_value: {set:?} → text input value={read_back:?}");

    // The control for the line above. An empty read-back has two causes — the write never landed,
    // or this engine never reports a web input's text — and only text that reached the field by
    // another route tells them apart. Typed through the pointer and keyboard, which do not touch
    // the accessibility write path.
    if let Some(field) = text_input(&after) {
        let focus = glass.click_element(field.id);
        let raised = glass.window(&WindowOp::Focus);
        let keyed = glass.key(&KeyEvent::Text(KEYED.to_string()));
        std::thread::sleep(Duration::from_millis(500));
        match glass.a11y_snapshot(Some(0)) {
            Ok(after) => println!(
                "control — click {focus:?}, focus {:?}, key {keyed:?} → text input value={:?}",
                raised.is_ok(),
                text_input(&after).and_then(|n| n.value.clone())
            ),
            Err(e) => println!("control — re-snapshot failed: {e}"),
        }
    }

    match glass.a11y_snapshot(Some(0)) {
        Ok(after) => {
            println!("every editable node at the end:");
            for node in collect(&after, &|n| n.states.editable) {
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
    (clicked, value_took)
}

/// Every `Document` control type under `root`, with the `FrameworkId` that is the whole point of
/// the reading. Returns the distinct framework ids seen, in the order first seen.
fn documents_under(automation: &UIAutomation, root: UIElement, label: &str) -> Vec<String> {
    let started = Instant::now();
    let found = automation
        .create_matcher()
        .from(root)
        .control_type(ControlType::Document)
        .depth(DOC_DEPTH)
        .find_all();
    let mut frameworks: Vec<String> = Vec::new();
    match found {
        Ok(elements) => {
            println!(
                "{label}: {} Document element(s) in {:?}",
                elements.len(),
                started.elapsed()
            );
            for el in &elements {
                let framework = el.get_framework_id().unwrap_or_default();
                println!(
                    "  Document: framework={framework:?} class={:?} name={:?} pid={:?}",
                    el.get_classname(),
                    el.get_name(),
                    el.get_process_id()
                );
                if !framework.is_empty() && !frameworks.contains(&framework) {
                    frameworks.push(framework);
                }
            }
        }
        // `find_all` reports "nothing matched" as an error, so this is the no-Document case too.
        Err(e) => println!(
            "{label}: no Document element after {:?} ({e})",
            started.elapsed()
        ),
    }
    frameworks
}

/// The app's top-level window as a UIA element, bound by handle exactly as the reader itself
/// binds it — so the scoped walk sees the window glass drives, not a peer app's.
fn app_window_element(glass: &mut Glass, automation: &UIAutomation) -> Option<UIElement> {
    let windows = match glass.list_windows() {
        Ok(w) => w,
        Err(e) => {
            println!("list_windows failed: {e}");
            return None;
        }
    };
    for w in &windows {
        println!(
            "window: handle=0x{:x} title={:?} class={:?} active={}",
            w.id.0, w.title, w.class, w.active
        );
    }
    let target = windows
        .iter()
        .find(|w| w.active)
        .or_else(|| windows.first())?;
    match automation.element_from_handle(Handle::from(target.id.0 as isize)) {
        Ok(el) => {
            println!(
                "window element: framework={:?} class={:?} name={:?} pid={:?}",
                el.get_framework_id(),
                el.get_classname(),
                el.get_name(),
                el.get_process_id()
            );
            Some(el)
        }
        Err(e) => {
            println!("element_from_handle failed: {e}");
            None
        }
    }
}

/// The `FrameworkId` reading for a running app: scoped to its own window first (precise), then
/// the whole-desktop walk the recipe calls for, which also shows what else on the box publishes
/// a `Document`. Returns the framework ids seen under the app's own window.
fn framework_reading(glass: &mut Glass, label: &str) -> Vec<String> {
    let automation = match UIAutomation::new() {
        Ok(a) => a,
        Err(e) => {
            println!("UIAutomation::new failed: {e}");
            return Vec::new();
        }
    };
    let scoped = match app_window_element(glass, &automation) {
        Some(window) => documents_under(&automation, window, &format!("{label}, from its window")),
        None => Vec::new(),
    };
    match automation.get_root_element() {
        Ok(root) => {
            documents_under(
                &automation,
                root,
                &format!("{label}, from the desktop root"),
            );
        }
        Err(e) => println!("get_root_element failed: {e}"),
    }
    scoped
}

/// Count processes named `exe` whose command line carries `marker` — our isolated profile — so
/// the box's own browsers are not counted. `-1` means the query itself failed.
fn our_process_count(exe: &str, marker: &str) -> i32 {
    let ps = format!(
        "@(Get-CimInstance Win32_Process -Filter \"Name='{exe}'\" | \
         Where-Object {{ $_.CommandLine -like '*{marker}*' }}).Count"
    );
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(-1),
        Err(_) => -1,
    }
}

/// One engine's whole reading: launch on the fixture, wait for content, actuate, then read the
/// `Document` control types. Never panics after `start` — `stop` has to run.
fn probe_browser(engine: &'static str, exe: &str, marker: &str) -> Reading {
    println!("\n=== {engine} — {exe} ===");
    let profile = glass_windows::onbox_support::scratch_dir(marker);
    let _ = std::fs::remove_dir_all(&profile);

    let spec = browser_spec(exe, &profile);
    println!("run: {:?}", spec.run);

    let mut reading = Reading {
        engine,
        installed: true,
        ..Reading::absent(engine)
    };
    let mut glass = glass_windows_with_a11y();
    let started = Instant::now();
    if let Err(e) = glass.start(&spec) {
        println!("start failed after {:?}: {e}", started.elapsed());
        let _ = std::fs::remove_dir_all(&profile);
        return reading;
    }
    println!("window mapped after {:?}", started.elapsed());

    let (tree, settle, arrived) = snapshot_until_page(&mut glass, SETTLE);
    println!("page content arrived within {SETTLE:?}: {arrived} after {settle:?}");
    let (tree, arrived) = if arrived {
        (tree, true)
    } else {
        // A second, longer read so "slow" is not recorded as "never".
        let (tree, settle, arrived) = snapshot_until_page(&mut glass, EXTENDED_SETTLE);
        println!("extended read: arrived={arrived} after a further {settle:?}");
        (tree, arrived)
    };
    reading.arrived = arrived;

    match tree {
        Some(tree) => {
            report_tree(&tree);
            if arrived {
                let (clicked, value_took) = exercise(&mut glass);
                reading.clicked = clicked;
                reading.value_took = value_took;
            } else if let Some(hint) = tree.document_guidance() {
                println!("disclosure rendered:\n{hint}");
            } else {
                println!("NO DOCUMENT DISCLOSURE — content missing and nothing said so");
            }
        }
        None => println!("no tree at all — nothing was published"),
    }

    reading.frameworks = framework_reading(&mut glass, engine);
    println!("stop: {:?}", glass.stop());
    let _ = std::fs::remove_dir_all(&profile);
    reading
}

/// The text-editor half of the `FrameworkId` reading: a stock editor's `Document` is the thing a
/// web document has to be told apart from.
fn probe_notepad() -> Vec<String> {
    println!("\n=== notepad ===");
    let mut glass = glass_windows_with_a11y();
    if let Err(e) = glass.start(&notepad_spec()) {
        println!("start notepad failed: {e}");
        return Vec::new();
    }
    let raised = glass.window(&WindowOp::Focus);
    let typed = glass.key(&KeyEvent::Text(NOTEPAD_LINE.to_string()));
    println!("focus {:?}, typed a line: {typed:?}", raised.is_ok());
    std::thread::sleep(Duration::from_millis(800));
    match glass.a11y_snapshot(Some(0)) {
        Ok(tree) => report_tree(&tree),
        Err(e) => println!("notepad snapshot failed: {e}"),
    }
    let frameworks = framework_reading(&mut glass, "notepad");
    println!("stop: {:?}", glass.stop());
    frameworks
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session and an installed browser"]
fn web_probe() {
    let mut readings = Vec::new();
    let mut launched = Vec::new();
    for (engine, relative) in BROWSERS {
        let Some(exe) = locate(relative) else {
            println!("\n=== {engine} — not installed at {relative}; skipped ===");
            readings.push(Reading::absent(engine));
            continue;
        };
        let stem = Path::new(&exe)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("browser")
            .to_string();
        let exe_name = Path::new(&exe)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let marker = format!("glass-web-probe-{stem}");
        readings.push(probe_browser(engine, &exe, &marker));
        launched.push((exe_name, marker));
    }
    let notepad = probe_notepad();

    println!("\n== aggregate: web-content probe ==");
    for r in &readings {
        if !r.installed {
            println!("  {}: unread (not installed)", r.engine);
            continue;
        }
        println!(
            "  {}: arrived={} clicked={} set_value_took={} frameworks={:?}",
            r.engine, r.arrived, r.clicked, r.value_took, r.frameworks
        );
    }
    println!("  notepad: frameworks={notepad:?}");

    // The only assertion: a browser this probe launched may not outlive it.
    let survivors: Vec<(String, i32)> = launched
        .iter()
        .map(|(exe, marker)| (exe.clone(), our_process_count(exe, marker)))
        .filter(|(_, n)| *n != 0)
        .collect();
    println!("  survivors after stop: {survivors:?}");
    assert!(
        survivors.is_empty(),
        "processes carrying our profile marker survived stop: {survivors:?}"
    );
}
