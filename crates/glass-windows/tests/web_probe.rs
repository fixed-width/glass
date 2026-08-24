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
//! that isn't installed prints a skip line. Every reading is also written to
//! `.windows-artifacts/web-probe.txt`, which the harness copies back — see [`transcript`].
//!
//! A probe, not a mapping test: it prints evidence and does not assert what an engine ought to
//! publish — a browser that starts but never shows the page's own content is a reading, not a
//! failure (`arrived: false`, plus whatever disclosure rendered). What does fail the run: a
//! browser that never launches, an accessibility channel that never answers, or a process this
//! probe launched that outlives both `stop` and its own cleanup kill.
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

/// The repo root the box built from, two levels above this crate's manifest dir.
fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above crates/glass-windows")
        .to_path_buf()
}

/// The transcript the harness ships back. `None` if the directory cannot be created.
fn transcript_path() -> Option<std::path::PathBuf> {
    let dir = repo_root().join(".windows-artifacts");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("web-probe.txt"))
}

/// Write one reading to stdout and to the transcript. libtest discards a passing test's stdout and
/// the harness runs the test binary without `--nocapture`, so the file is the only copy of the
/// reading that survives a green run.
fn transcript(line: &str) {
    println!("{line}");
    let Some(path) = transcript_path() else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{line}");
    }
}

/// The probe's only output call — see [`transcript`].
macro_rules! say {
    ($($arg:tt)*) => { transcript(&format!($($arg)*)) };
}

/// The shared web fixture as a `file://` URL, derived from this crate's manifest dir so it points
/// at whatever checkout the box built. Backslashes are swapped for forward slashes: a browser
/// takes the URL form, not the Windows path form.
fn page_url() -> String {
    let path = repo_root()
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

/// Gecko's onboarding, turned off in the profile. Read on 2026-08-24: a fresh Firefox profile
/// renders `about:welcome` over the requested page, so the reader's subject is the onboarding
/// content and the fixture is never read. `--profile` is honoured before these are needed, and
/// `user.js` is applied on every startup, so writing it into the fresh directory is enough.
const GECKO_PREFS: &str = r#"user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.migrate.content-modal.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.startup.upgradeDialog.enabled", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
"#;

/// Create the profile directory, seeding a Gecko one with [`GECKO_PREFS`].
fn prepare_profile(exe: &str, profile: &str) {
    if let Err(e) = std::fs::create_dir_all(profile) {
        say!("could not create the profile dir {profile}: {e}");
        return;
    }
    if is_gecko(exe) {
        let path = Path::new(profile).join("user.js");
        if let Err(e) = std::fs::write(&path, GECKO_PREFS) {
            say!("could not write {}: {e}", path.display());
        }
    }
}

fn is_gecko(exe: &str) -> bool {
    exe.to_ascii_lowercase().contains("firefox")
}

/// A fresh, isolated profile per launch: no first-run prompts, no session restore, and no
/// already-running instance adopting the URL instead of starting a process glass owns. The
/// profile path carries `marker`, which is also how the teardown check finds our processes.
fn browser_spec(exe: &str, profile: &str) -> AppSpec {
    let run = if is_gecko(exe) {
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
/// once and is recorded into `failures` — a caller not yet answering is expected during startup,
/// but every other error is evidence the accessibility channel itself broke.
fn snapshot_until_page(
    glass: &mut Glass,
    budget: Duration,
    label: &str,
    failures: &mut Vec<String>,
) -> (Option<AxTree>, Duration, bool) {
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
                    say!("snapshot error at {:?}: {text}", start.elapsed());
                    failures.push(format!("{label}: snapshot error: {text}"));
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
        say!(
            "document: #{} role={:?} raw_role={:?} name={:?} children={} bounds={:?}",
            doc.id.0,
            doc.role,
            doc.raw_role,
            doc.name,
            doc.children.len(),
            doc.bounds
        );
    }
    say!(
        "heading {HEADING:?} present: {}",
        find(tree, &|n| n.role == AxRole::Heading
            && n.name.as_deref() == Some(HEADING))
        .is_some()
    );
    say!("role histogram (role, count, native token):");
    for tally in role_histogram(tree) {
        say!("  {:?} {:>4}  {}", tally.role, tally.count, tally.raw_role);
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
        say!("notice: {notice}");
    }
    say!("outline:\n{}", tree.to_outline());
}

/// How many times to re-snapshot-and-click. A browser's tree keeps changing while the page
/// settles, and an id whose node has moved since the snapshot is rejected as changed.
const CLICK_ATTEMPTS: usize = 4;

/// Push a formatted `{label}: {context}: {e}` into `failures` unless `e` is
/// [`GlassError::AccessibilityNotReady`] — a caller not yet answering is expected during
/// startup and settles on retry; every other error means the accessibility channel broke after
/// it had already been serving this session.
fn record_snapshot_failure(failures: &mut Vec<String>, label: &str, context: &str, e: &GlassError) {
    if !matches!(e, GlassError::AccessibilityNotReady(_)) {
        failures.push(format!("{label}: {context}: {e}"));
    }
}

/// The actuation readings. Each step re-snapshots first: `click_element` and `set_value` resolve
/// ids against the session's most recent tree, so an id from an older one addresses nothing.
/// Returns whether the click landed and whether the written value read back.
fn exercise(glass: &mut Glass, label: &str, failures: &mut Vec<String>) -> (bool, bool) {
    let mut clicked = false;
    for attempt in 1..=CLICK_ATTEMPTS {
        let before = match glass.a11y_snapshot(Some(0)) {
            Ok(tree) => tree,
            Err(e) => {
                say!("snapshot before the click failed: {e} — no click reading");
                record_snapshot_failure(failures, label, "snapshot before the click failed", &e);
                return (false, false);
            }
        };
        let Some(button) = named(&before, BUTTON) else {
            say!("no node named {BUTTON:?} — no click reading");
            return (false, false);
        };
        if attempt == 1 {
            say!(
                "button: #{} role={:?} raw_role={} bounds={:?}",
                button.id.0,
                button.role,
                button.raw_role,
                button.bounds
            );
        }
        match glass.click_element(button.id) {
            Ok(method) => {
                std::thread::sleep(Duration::from_millis(500));
                match glass.a11y_snapshot(Some(0)) {
                    Ok(after) => {
                        clicked = carrying(&after, CLICKED).is_some();
                        say!(
                            "click_element (attempt {attempt}): {method:?} → result paragraph \
                             reads {CLICKED:?}: {clicked}"
                        );
                    }
                    Err(e) => {
                        say!("click_element: {method:?} → re-snapshot failed: {e}");
                        record_snapshot_failure(
                            failures,
                            label,
                            "re-snapshot after click_element failed",
                            &e,
                        );
                    }
                }
                break;
            }
            Err(e) => {
                say!("click_element (attempt {attempt}) failed: {e}");
                record_snapshot_failure(failures, label, "click_element failed", &e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    let after = match glass.a11y_snapshot(Some(0)) {
        Ok(tree) => tree,
        Err(e) => {
            say!("snapshot before set_value failed: {e} — no set_value reading");
            record_snapshot_failure(failures, label, "snapshot before set_value failed", &e);
            return (clicked, false);
        }
    };
    let Some(field) = text_input(&after) else {
        say!("no editable node named {INPUT:?} — no set_value reading");
        return (clicked, false);
    };
    say!(
        "text input: #{} role={:?} raw_role={} value={:?}",
        field.id.0,
        field.role,
        field.raw_role,
        field.value
    );
    let set = glass.set_value(field.id, TYPED);
    std::thread::sleep(Duration::from_millis(500));
    let after = match glass.a11y_snapshot(Some(0)) {
        Ok(tree) => tree,
        Err(e) => {
            say!("set_value: {set:?} → re-snapshot failed: {e}");
            record_snapshot_failure(failures, label, "re-snapshot after set_value failed", &e);
            return (clicked, false);
        }
    };
    let read_back = text_input(&after).and_then(|n| n.value.clone());
    let value_took = read_back.as_deref() == Some(TYPED);
    say!("set_value: {set:?} → text input value={read_back:?}");

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
            Ok(after) => say!(
                "control — click {focus:?}, focus {:?}, key {keyed:?} → text input value={:?}",
                raised.is_ok(),
                text_input(&after).and_then(|n| n.value.clone())
            ),
            Err(e) => {
                say!("control — re-snapshot failed: {e}");
                record_snapshot_failure(failures, label, "control re-snapshot failed", &e);
            }
        }
    }

    match glass.a11y_snapshot(Some(0)) {
        Ok(after) => {
            say!("every editable node at the end:");
            for node in collect(&after, &|n| n.states.editable) {
                say!(
                    "  #{} role={:?} raw_role={} name={:?} value={:?}",
                    node.id.0,
                    node.role,
                    node.raw_role,
                    node.name,
                    node.value
                );
            }
            say!("--- tree after actuation ---");
            report_tree(&after);
        }
        Err(e) => {
            say!("final snapshot failed: {e}");
            record_snapshot_failure(failures, label, "final snapshot failed", &e);
        }
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
            say!(
                "{label}: {} Document element(s) in {:?}",
                elements.len(),
                started.elapsed()
            );
            for el in &elements {
                let framework = el.get_framework_id().unwrap_or_default();
                say!(
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
        Err(e) => say!(
            "{label}: no Document element after {:?} ({e})",
            started.elapsed()
        ),
    }
    frameworks
}

/// The app's top-level window as a UIA element, bound by handle exactly as the reader itself
/// binds it — so the scoped walk sees the window glass drives, not a peer app's.
fn app_window_element(
    glass: &mut Glass,
    automation: &UIAutomation,
    label: &str,
    failures: &mut Vec<String>,
) -> Option<UIElement> {
    let windows = match glass.list_windows() {
        Ok(w) => w,
        Err(e) => {
            say!("list_windows failed: {e}");
            record_snapshot_failure(failures, label, "list_windows failed", &e);
            return None;
        }
    };
    if windows.is_empty() {
        say!(
            "list_windows returned no window — the adopted window's process is not in the pid set glass tracks, so there is no scoped walk"
        );
    }
    for w in &windows {
        say!(
            "window: handle=0x{:x} title={:?} class={:?} active={}",
            w.id.0,
            w.title,
            w.class,
            w.active
        );
    }
    let target = windows
        .iter()
        .find(|w| w.active)
        .or_else(|| windows.first())?;
    match automation.element_from_handle(Handle::from(target.id.0 as isize)) {
        Ok(el) => {
            say!(
                "window element: framework={:?} class={:?} name={:?} pid={:?}",
                el.get_framework_id(),
                el.get_classname(),
                el.get_name(),
                el.get_process_id()
            );
            Some(el)
        }
        Err(e) => {
            say!("element_from_handle failed: {e}");
            failures.push(format!("{label}: element_from_handle failed: {e}"));
            None
        }
    }
}

/// The `FrameworkId` reading for a running app: scoped to its own window first (precise), then
/// the whole-desktop walk the recipe calls for, which also shows what else on the box publishes
/// a `Document`. Returns the framework ids seen under the app's own window.
fn framework_reading(glass: &mut Glass, label: &str, failures: &mut Vec<String>) -> Vec<String> {
    let automation = match UIAutomation::new() {
        Ok(a) => a,
        Err(e) => {
            say!("UIAutomation::new failed: {e}");
            failures.push(format!("{label}: UIAutomation::new failed: {e}"));
            return Vec::new();
        }
    };
    let scoped = match app_window_element(glass, &automation, label, failures) {
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
        Err(e) => {
            say!("get_root_element failed: {e}");
            failures.push(format!("{label}: get_root_element failed: {e}"));
        }
    }
    scoped
}

/// Pids of processes named `exe` whose command line carries `marker` — our isolated profile — so
/// the box's own browsers are never matched. An empty vec also covers a failed query, which is
/// why the teardown check kills what this returns rather than trusting a count — and why a
/// failed query is recorded into `failures` rather than trusted silently: a query that never
/// works would otherwise report every survivor as "gone" for the rest of the run.
fn our_pids(exe: &str, marker: &str, failures: &mut Vec<String>) -> Vec<u32> {
    let ps = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='{exe}'\" | \
         Where-Object {{ $_.CommandLine -like '*{marker}*' }} | \
         ForEach-Object {{ $_.ProcessId }}"
    );
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect(),
        Err(e) => {
            say!("pid query for {exe} failed: {e}");
            let msg = format!("{exe}: pid query failed: {e}");
            if !failures.contains(&msg) {
                failures.push(msg);
            }
            Vec::new()
        }
    }
}

/// Poll until nothing carrying `marker` is left or `budget` elapses, returning what is still
/// there. A browser tree takes seconds to unwind, so an immediate read would call a normal
/// shutdown a leak.
fn wait_for_no_process(
    exe: &str,
    marker: &str,
    budget: Duration,
    failures: &mut Vec<String>,
) -> Vec<u32> {
    let deadline = Instant::now() + budget;
    loop {
        let pids = our_pids(exe, marker, failures);
        if pids.is_empty() || Instant::now() >= deadline {
            return pids;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// How long teardown is given before a survivor counts as one.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(15);

/// One engine's whole reading: launch on the fixture, wait for content, actuate, then read the
/// `Document` control types. Never panics after `start` — `stop` has to run; genuine breakage is
/// recorded into `failures` instead, for the test to panic on once every browser has had its turn.
fn probe_browser(
    engine: &'static str,
    exe: &str,
    marker: &str,
    failures: &mut Vec<String>,
) -> Reading {
    say!("\n=== {engine} — {exe} ===");
    let profile = glass_windows::onbox_support::scratch_dir(marker);
    let _ = std::fs::remove_dir_all(&profile);
    prepare_profile(exe, &profile);

    let spec = browser_spec(exe, &profile);
    say!("run: {:?}", spec.run);

    let mut reading = Reading {
        engine,
        installed: true,
        ..Reading::absent(engine)
    };
    let mut glass = glass_windows_with_a11y();
    let started = Instant::now();
    if let Err(e) = glass.start(&spec) {
        say!("start failed after {:?}: {e}", started.elapsed());
        failures.push(format!(
            "{engine}: start failed after {:?}: {e}",
            started.elapsed()
        ));
        let _ = std::fs::remove_dir_all(&profile);
        return reading;
    }
    say!("window mapped after {:?}", started.elapsed());

    let (tree, settle, arrived) = snapshot_until_page(&mut glass, SETTLE, engine, failures);
    say!("page content arrived within {SETTLE:?}: {arrived} after {settle:?}");
    let (tree, arrived) = if arrived {
        (tree, true)
    } else {
        // A second, longer read so "slow" is not recorded as "never".
        let (tree, settle, arrived) =
            snapshot_until_page(&mut glass, EXTENDED_SETTLE, engine, failures);
        say!("extended read: arrived={arrived} after a further {settle:?}");
        (tree, arrived)
    };
    reading.arrived = arrived;

    match tree {
        Some(tree) => {
            report_tree(&tree);
            if arrived {
                let (clicked, value_took) = exercise(&mut glass, engine, failures);
                reading.clicked = clicked;
                reading.value_took = value_took;
            } else if let Some(hint) = tree.document_guidance() {
                say!("disclosure rendered:\n{hint}");
            } else {
                say!("NO DOCUMENT DISCLOSURE — content missing and nothing said so");
            }
        }
        None => say!("no tree at all — nothing was published"),
    }

    reading.frameworks = framework_reading(&mut glass, engine, failures);
    say!("stop: {:?}", glass.stop());
    let _ = std::fs::remove_dir_all(&profile);
    reading
}

/// The text-editor half of the `FrameworkId` reading: a stock editor's `Document` is the thing a
/// web document has to be told apart from.
fn probe_notepad(failures: &mut Vec<String>) -> Vec<String> {
    say!("\n=== notepad ===");
    let mut glass = glass_windows_with_a11y();
    if let Err(e) = glass.start(&notepad_spec()) {
        say!("start notepad failed: {e}");
        failures.push(format!("notepad: start failed: {e}"));
        return Vec::new();
    }
    let raised = glass.window(&WindowOp::Focus);
    let typed = glass.key(&KeyEvent::Text(NOTEPAD_LINE.to_string()));
    say!("focus {:?}, typed a line: {typed:?}", raised.is_ok());
    std::thread::sleep(Duration::from_millis(800));
    match glass.a11y_snapshot(Some(0)) {
        Ok(tree) => report_tree(&tree),
        Err(e) => {
            say!("notepad snapshot failed: {e}");
            record_snapshot_failure(failures, "notepad", "snapshot failed", &e);
        }
    }
    let frameworks = framework_reading(&mut glass, "notepad", failures);
    say!("stop: {:?}", glass.stop());
    frameworks
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session and an installed browser"]
fn web_probe() {
    let mut readings = Vec::new();
    let mut launched = Vec::new();
    // Breakage in the accessibility channel itself, not what a browser published — collected
    // per browser so every requested browser still gets its turn (and its `stop`), with the run
    // failing once at the end rather than on whichever browser happened to hit it first.
    let mut failures: Vec<String> = Vec::new();
    for (engine, relative) in BROWSERS {
        let Some(exe) = locate(relative) else {
            say!("\n=== {engine} — not installed at {relative}; skipped ===");
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
        readings.push(probe_browser(engine, &exe, &marker, &mut failures));
        launched.push((exe_name, marker));
    }
    let notepad = probe_notepad(&mut failures);

    say!("\n== aggregate: web-content probe ==");
    for r in &readings {
        if !r.installed {
            say!("  {}: unread (not installed)", r.engine);
            continue;
        }
        say!(
            "  {}: arrived={} clicked={} set_value_took={} frameworks={:?}",
            r.engine,
            r.arrived,
            r.clicked,
            r.value_took,
            r.frameworks
        );
    }
    say!("  notepad: frameworks={notepad:?}");

    // Whether `stop` reached each browser is a reading, printed above the cleanup so a leak is
    // visible even though the probe then repairs it. The probe owns every process carrying its
    // own profile marker, so it kills those by exact pid rather than leaving them on the box.
    let mut left_behind = Vec::new();
    for (exe, marker) in &launched {
        let survivors = wait_for_no_process(exe, marker, TEARDOWN_BUDGET, &mut failures);
        say!("  survived stop by {TEARDOWN_BUDGET:?} — {exe}: {survivors:?}");
        for pid in &survivors {
            let killed = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output()
                .is_ok();
            say!("  killed {exe} pid {pid}: {killed}");
        }
        if !survivors.is_empty() {
            left_behind.extend(wait_for_no_process(
                exe,
                marker,
                TEARDOWN_BUDGET,
                &mut failures,
            ));
        }
        let _ = std::fs::remove_dir_all(glass_windows::onbox_support::scratch_dir(marker));
    }

    // Nothing this probe launched may still be running on the box.
    assert!(
        left_behind.is_empty(),
        "processes carrying our profile marker outlived both stop and the probe's own kill: \
         {left_behind:?}"
    );
    // Genuine breakage — a browser that never launched, or an accessibility channel that never
    // answered — collected while every browser still got its turn; asserted last, so the
    // teardown above always runs first.
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
