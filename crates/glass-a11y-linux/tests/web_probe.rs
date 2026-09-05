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
//! publish — a browser that launches but shows no page content is a reading, not a failure
//! (`arrived: false` plus whatever disclosure rendered). What does fail the run: a browser that
//! never launches, an accessibility bus that never answers at all, or a `set_value` whose verdict
//! disagrees with the value the field reads back — the last is a claim about glass, which owes the
//! same answer on every engine. Each backend's `Drop` impl tears its session down even through a
//! panic, so failures are collected per (backend, browser, lever) and the test panics once at the
//! end, after every browser it started has already been stopped.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use glass_core::{
    ActionMethod, ActionMode, ActionTarget, ActionabilityCheckName, ActionabilityReport,
    ActionabilityVerdict, AppSpec, AxNode, AxRole, AxTree, Backend, BaselineStore,
    ClickTargetParams, DispatchStatus, ElementCondition, FindElementsParams, Glass, GlassError,
    PlatformFactory, SandboxLevel, ScopeResolution, SemanticActionFailureKind, SemanticQuery,
    SemanticSelector, SemanticState, SemanticTarget, TypeTargetParams, WaitElementParams,
    WindowHint, role_histogram,
};

const BROWSERS_VAR: &str = "GLASS_WEB_PROBE_BROWSERS";
const LEVER_VAR: &str = "GLASS_WEB_PROBE_LEVER";

/// How long to keep re-reading the tree for the page's own elements before calling the content
/// missing. The reading this bounds is "time to content", so it is deliberately generous.
const SETTLE: Duration = Duration::from_secs(20);
const SEMANTIC_READ_TIMEOUT_MS: u64 = 20_000;

/// Window-discovery budget. A cold browser opening a fresh profile under a software renderer
/// maps its window far later than the GTK fixture does.
const LAUNCH_TIMEOUT_MS: u64 = 30_000;

/// The fixture button's accessible name — the page content the probe waits for, then clicks.
const BUTTON: &str = "click me";

fn page_url() -> String {
    format!(
        "file://{}/../../examples/web-role-fixture/index.html",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn is_gecko(browser: &str) -> bool {
    browser.contains("firefox")
}

/// Gecko's onboarding, turned off in the profile. A fresh Firefox profile renders
/// `about:welcome` over the requested page, so the reader's subject is the onboarding content
/// and the fixture is never read. `--profile` is honoured before these are needed, and
/// `user.js` is applied on every startup, so writing it into the fresh directory is enough.
/// Matches the Windows probe's `GECKO_PREFS` so both probes see the same browser.
const GECKO_PREFS: &str = r#"user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.migrate.content-modal.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.startup.upgradeDialog.enabled", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
"#;

/// Seed a fresh Gecko profile with [`GECKO_PREFS`]; a no-op for every other engine.
fn prepare_profile(browser: &str, profile: &Path) {
    if !is_gecko(browser) {
        return;
    }
    let path = profile.join("user.js");
    if let Err(e) = std::fs::write(&path, GECKO_PREFS) {
        println!("could not write {}: {e}", path.display());
    }
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
    /// # Panics
    ///
    /// Panics when the variable is set to a value (trimmed) that names none of the accepted
    /// levers. Falling back to the baseline on a typo would silently run the probe without the
    /// lever the caller meant to test.
    fn from_env() -> Self {
        let raw = std::env::var(LEVER_VAR).unwrap_or_default();
        match raw.trim() {
            "" => Lever::None,
            "1" | "both" => Lever::Both,
            "gnome" => Lever::Gnome,
            "enabled" => Lever::Enabled,
            "flag" => Lever::RendererFlag,
            other => panic!(
                "{LEVER_VAR}={other:?} is not a recognised lever — set it to one of: 1, both, \
                 gnome, enabled, flag, or leave it unset for the baseline reading"
            ),
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
    let run = if is_gecko(browser) {
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
/// once the second one registers. Each distinct error is printed once and recorded into
/// `failures` — a caller not yet answering is expected during startup, but every other error is
/// evidence the a11y bus itself broke, not a reading about the page.
fn snapshot_until_page(
    glass: &mut Glass,
    label: &str,
    failures: &mut Vec<String>,
) -> (Option<AxTree>, Duration, bool) {
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
                    failures.push(format!("{label}: snapshot error: {text}"));
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
        tree.unexposed_notice(),
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

/// Discover the large fixture's account controls before any explicit unbounded diagnostic
/// snapshot. Browser non-publication remains evidence-only, while malformed published content is
/// recorded as a probe failure.
fn account_scope() -> SemanticTarget {
    SemanticTarget {
        target: SemanticSelector::new(
            Some("Account name".into()),
            Some(AxRole::TextField),
            vec![SemanticState::Enabled],
        )
        .expect("account selector"),
        within: Some(
            SemanticSelector::new(
                Some("Glass web fixture".into()),
                Some(AxRole::Document),
                Vec::new(),
            )
            .expect("document scope"),
        ),
    }
}

fn save_account_target() -> SemanticTarget {
    SemanticTarget {
        target: SemanticSelector::new(
            Some("Save account".into()),
            Some(AxRole::Button),
            vec![SemanticState::Enabled],
        )
        .expect("save selector"),
        within: account_scope().within,
    }
}

fn usable_account_controls_are_published(glass: &mut Glass) -> bool {
    let within = SemanticSelector::new(
        Some("Glass web fixture".into()),
        Some(AxRole::Document),
        Vec::new(),
    )
    .expect("document scope");
    let account = SemanticQuery::new(
        SemanticSelector::new(
            Some("Account name".into()),
            Some(AxRole::TextField),
            vec![SemanticState::Enabled],
        )
        .expect("account selector"),
        Some(within),
        1,
    )
    .expect("account query");
    glass
        .find_elements(&FindElementsParams {
            query: account,
            max_nodes: None,
            timeout_ms: 0,
        })
        .is_ok_and(|outcome| {
            matches!(outcome.result.scope, ScopeResolution::Resolved(_))
                && outcome.result.matches.len() == 1
        })
}

/// Run the account mutation entirely through fresh semantic targets before any diagnostic tree
/// snapshot. Returns false only when the browser published no usable Account controls at all.
fn exercise_account_semantically(
    glass: &mut Glass,
    label: &str,
    failures: &mut Vec<String>,
) -> bool {
    let started = Instant::now();
    let typed = glass.type_target(
        &TypeTargetParams {
            target: account_scope(),
            focus_mode: ActionMode::Native,
            timeout_ms: 20_000,
            max_nodes: None,
        },
        "Ada",
    );
    let typed = match typed {
        Ok(outcome) => outcome,
        Err(error) => {
            println!(
                "semantic Account targeted type failed after {:?}: kind={} dispatch={} \
                 error={error:?}",
                started.elapsed(),
                error.kind.as_str(),
                error.action_dispatch.as_str(),
            );
            if !usable_account_controls_are_published(glass) {
                println!("no usable page controls published — preserving evidence-only behavior");
                return false;
            }
            failures.push(format!(
                "{label}: semantic Account targeted type failed with usable controls published: \
                 kind={} dispatch={} error={error:?}",
                error.kind.as_str(),
                error.action_dispatch.as_str(),
            ));
            return true;
        }
    };
    let focus = typed.focus.as_ref().expect("targeted type focus report");
    println!(
        "semantic Account targeted type after {:?}: focus_method={} focus_dispatch={} \
         focus_confirmation={} type_method={} type_dispatch={} type_confirmation={} \
         actionability={:?}",
        started.elapsed(),
        focus.method.as_str(),
        focus.dispatch.as_str(),
        focus.confirmation.as_str(),
        typed.action.method.as_str(),
        typed.action.dispatch.as_str(),
        typed.action.confirmation.as_str(),
        typed.actionability.checks,
    );

    let field = glass
        .wait_for_element(&WaitElementParams {
            name: Some("Account name".into()),
            description: None,
            role: Some(AxRole::TextField),
            value: Some("Ada".into()),
            value_contains: None,
            condition: ElementCondition::Appears,
            interval_ms: 25,
            timeout_ms: SEMANTIC_READ_TIMEOUT_MS,
        })
        .expect("wait for Account value");
    if !field.matched {
        failures.push(format!("{label}: Account field did not read back as Ada"));
        return true;
    }

    let clicked_at = Instant::now();
    let click = match glass.click_target(&ClickTargetParams {
        target: ActionTarget::Semantic(save_account_target()),
        mode: ActionMode::Auto,
        timeout_ms: Some(20_000),
        max_nodes: None,
    }) {
        Ok(outcome) => outcome,
        Err(error) => {
            failures.push(format!(
                "{label}: semantic Save account click failed: kind={} dispatch={} error={error:?}",
                error.kind.as_str(),
                error.action_dispatch.as_str(),
            ));
            return true;
        }
    };
    println!(
        "semantic Save account click after {:?}: method={} dispatch={} confirmation={} actionability={:?}",
        clicked_at.elapsed(),
        click.action.method.as_str(),
        click.action.dispatch.as_str(),
        click.action.confirmation.as_str(),
        click.actionability.checks,
    );
    let saved = glass
        .wait_for_element(&WaitElementParams {
            name: None,
            description: None,
            role: Some(AxRole::TextArea),
            value: Some("Saved".into()),
            value_contains: None,
            condition: ElementCondition::Appears,
            interval_ms: 25,
            timeout_ms: SEMANTIC_READ_TIMEOUT_MS,
        })
        .expect("wait for Saved status");
    if !saved.matched {
        failures.push(format!("{label}: Save click did not produce Saved status"));
    }
    println!(
        "semantic Account workflow: field=Ada status=Saved matched={}",
        saved.matched
    );
    true
}

fn fixture_target(name: &str, states: Vec<SemanticState>) -> SemanticTarget {
    SemanticTarget {
        target: SemanticSelector::new(Some(name.into()), Some(AxRole::Button), states)
            .expect("fixture selector"),
        within: account_scope().within,
    }
}

fn fixture_click(name: &str, mode: ActionMode, timeout_ms: u64) -> ClickTargetParams {
    ClickTargetParams {
        target: ActionTarget::Semantic(fixture_target(name, Vec::new())),
        mode,
        timeout_ms: Some(timeout_ms),
        max_nodes: None,
    }
}

fn actionability_verdict(
    report: &ActionabilityReport,
    name: ActionabilityCheckName,
) -> ActionabilityVerdict {
    report
        .checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| {
            panic!(
                "missing {name:?} actionability check in {:?}",
                report.checks
            )
        })
        .verdict
}

fn status_is(glass: &mut Glass, expected: &str) -> bool {
    glass
        .wait_for_element(&WaitElementParams {
            name: None,
            description: None,
            role: None,
            value: Some(expected.into()),
            value_contains: None,
            condition: ElementCondition::Appears,
            interval_ms: 25,
            timeout_ms: SEMANTIC_READ_TIMEOUT_MS,
        })
        .expect("semantic status read")
        .matched
}

fn assert_quiet_status(glass: &mut Glass, expected: &str, context: &str) {
    std::thread::sleep(Duration::from_millis(350));
    assert!(
        status_is(glass, expected),
        "{context} changed status during the 350 ms quiet window"
    );
}

fn exercise_actionability(glass: &mut Glass) {
    const INITIAL: &str = "No semantic action activated";
    assert!(status_is(glass, INITIAL));

    let disabled_started = Instant::now();
    let disabled = glass
        .click_target(&fixture_click("Disabled semantic", ActionMode::Native, 0))
        .expect_err("disabled target must be refused");
    let disabled_elapsed = disabled_started.elapsed();
    assert_eq!(disabled.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(disabled.action_dispatch, DispatchStatus::NotDispatched);
    assert_quiet_status(glass, INITIAL, "disabled refusal");
    println!(
        "disabled refusal after {:?}: kind={} dispatch={} actionability={:?}; quiet_ms=350",
        disabled_elapsed,
        disabled.kind.as_str(),
        disabled.action_dispatch.as_str(),
        disabled.actionability.checks,
    );

    let duplicate_started = Instant::now();
    let duplicate = glass
        .click_target(&fixture_click("Duplicate semantic", ActionMode::Native, 0))
        .expect_err("duplicate target must be refused");
    let duplicate_elapsed = duplicate_started.elapsed();
    assert_eq!(duplicate.kind, SemanticActionFailureKind::AmbiguousTarget);
    assert_eq!(duplicate.action_dispatch, DispatchStatus::NotDispatched);
    assert_quiet_status(glass, INITIAL, "ambiguous refusal");
    println!(
        "duplicate refusal after {:?}: kind={} dispatch={}; quiet_ms=350",
        duplicate_elapsed,
        duplicate.kind.as_str(),
        duplicate.action_dispatch.as_str(),
    );

    let delayed_query = SemanticQuery::new(
        fixture_target("Delayed semantic", vec![SemanticState::Enabled]).target,
        account_scope().within,
        1,
    )
    .expect("delayed query");
    let absent = glass
        .find_elements(&FindElementsParams {
            query: delayed_query,
            max_nodes: None,
            timeout_ms: 0,
        })
        .expect("one fresh delayed absence read");
    assert!(
        !absent.matched,
        "delayed target was not absent before its action"
    );
    assert_eq!(absent.result.matches_in_walk, 0);
    assert!(absent.result.search_complete);
    glass
        .click_target(&fixture_click("Start delay", ActionMode::Native, 20_000))
        .expect("start delayed publication");
    let delayed_started = Instant::now();
    let delayed = glass
        .click_target(&fixture_click("Delayed semantic", ActionMode::Auto, 20_000))
        .expect("delayed selector action waits and clicks");
    let delayed_elapsed = delayed_started.elapsed();
    assert_eq!(delayed.action.dispatch, DispatchStatus::Dispatched);
    assert!(status_is(glass, "Delayed activated 1"));
    assert_quiet_status(glass, "Delayed activated 1", "delayed activation");
    println!(
        "delayed action after observed absence waited {:?}: method={} dispatch={} confirmation={} \
         actionability={:?}; exact_activations=1",
        delayed_elapsed,
        delayed.action.method.as_str(),
        delayed.action.dispatch.as_str(),
        delayed.action.confirmation.as_str(),
        delayed.actionability.checks,
    );

    glass
        .click_target(&fixture_click("Start motion", ActionMode::Native, 20_000))
        .expect("restart motion");
    let first_tree = glass.a11y_snapshot(Some(0)).expect("first moving sample");
    let first_bounds = named(&first_tree, "Moving semantic")
        .and_then(|node| node.bounds)
        .expect("first moving bounds");
    let observed_deadline = Instant::now() + Duration::from_millis(250);
    let changed_bounds = loop {
        let tree = glass
            .a11y_snapshot(Some(0))
            .expect("changing moving sample");
        let bounds = named(&tree, "Moving semantic")
            .and_then(|node| node.bounds)
            .expect("changing moving bounds");
        if bounds != first_bounds {
            break bounds;
        }
        assert!(
            Instant::now() < observed_deadline,
            "moving target bounds never changed after Start motion"
        );
    };
    let moving_started = Instant::now();
    let moving = glass
        .click_target(&fixture_click(
            "Moving semantic",
            ActionMode::Pointer,
            20_000,
        ))
        .expect("forced pointer waits for stable bounds and clicks");
    let moving_elapsed = moving_started.elapsed();
    assert_eq!(
        moving.action.method,
        ActionMethod::Pointer {
            native_fallback: None
        }
    );
    assert_eq!(moving.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        actionability_verdict(&moving.actionability, ActionabilityCheckName::Stable),
        ActionabilityVerdict::Passed
    );
    assert!(status_is(glass, "Moving activated 1"));
    assert_quiet_status(glass, "Moving activated 1", "moving activation");
    println!(
        "moving generation bounds changed {first_bounds:?} -> {changed_bounds:?}; pointer waited \
         {:?}: method={} dispatch={} confirmation={} actionability={:?}; exact_activations=1",
        moving_elapsed,
        moving.action.method.as_str(),
        moving.action.dispatch.as_str(),
        moving.action.confirmation.as_str(),
        moving.actionability.checks,
    );

    let occlusion_tree = glass
        .a11y_snapshot(Some(0))
        .expect("occlusion identity snapshot");
    let occluded = named(&occlusion_tree, "Occluded semantic").expect("occluded target");
    let occluder = named(&occlusion_tree, "Occluder").expect("occluder identity");
    assert_eq!(occluder.role, AxRole::Button);
    let target_bounds = occluded.bounds.expect("occluded bounds");
    let cover_bounds = occluder.bounds.expect("occluder bounds");
    let center = (
        target_bounds.x + target_bounds.width as i32 / 2,
        target_bounds.y + target_bounds.height as i32 / 2,
    );
    assert!(
        center.0 >= cover_bounds.x
            && center.0 < cover_bounds.x + cover_bounds.width as i32
            && center.1 >= cover_bounds.y
            && center.1 < cover_bounds.y + cover_bounds.height as i32,
        "named Occluder does not cover the target center: target={target_bounds:?} \
         occluder={cover_bounds:?}"
    );
    let occluded_started = Instant::now();
    let refusal = glass
        .click_target(&fixture_click(
            "Occluded semantic",
            ActionMode::Pointer,
            20_000,
        ))
        .expect_err("AT-SPI must prove the named occluder before pointer dispatch");
    let occluded_elapsed = occluded_started.elapsed();
    println!("occlusion outcome before assertions: {refusal:?}");
    assert_eq!(refusal.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(refusal.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(
        actionability_verdict(&refusal.actionability, ActionabilityCheckName::NonOccluded),
        ActionabilityVerdict::Failed
    );
    assert_quiet_status(glass, "Moving activated 1", "occlusion refusal");
    println!(
        "occlusion refusal after {:?}: target_center={center:?} named_occluder=#{} {:?} \
         kind={} dispatch={} actionability={:?}; quiet_ms=350",
        occluded_elapsed,
        occluder.id.0,
        cover_bounds,
        refusal.kind.as_str(),
        refusal.action_dispatch.as_str(),
        refusal.actionability.checks,
    );
}

/// One backend/browser/lever combination. Failures that mean the a11y bus itself broke — a
/// launch that never happened, or a snapshot channel that never answered — are appended to
/// `failures` rather than panicking, so the rest of the requested browsers still get their turn
/// and this browser's `stop` still runs.
fn probe(backend: &str, browser: &str, lever: Lever, failures: &mut Vec<String>) {
    let failures_before = failures.len();
    let label = format!("{backend}/{browser}/lever={lever:?}");
    println!("=== {label} ===");
    println!("engine: {}", version_of(browser));

    let profile = tempfile::tempdir().expect("profile dir");
    prepare_profile(browser, profile.path());
    let spec = browser_spec(browser, profile.path(), lever);
    println!("run: {:?}", spec.run);
    println!("env: {:?}", spec.env);

    let mut glass = glass_for(backend);
    let started = Instant::now();
    if let Err(e) = glass.start(&spec) {
        println!("start failed after {:?}: {e}", started.elapsed());
        failures.push(format!(
            "{label}: start failed after {:?}: {e}",
            started.elapsed()
        ));
        return;
    }
    println!("window mapped after {:?}", started.elapsed());

    let account_controls_published = exercise_account_semantically(&mut glass, &label, failures);
    let (tree, settle, arrived) = snapshot_until_page(&mut glass, &label, failures);
    println!("page content arrived: {arrived} after {settle:?}");
    match tree {
        Some(tree) => {
            report_tree(&tree);
            if arrived && account_controls_published && failures.len() == failures_before {
                exercise_actionability(&mut glass);
            } else if let Some(hint) = tree.document_guidance().or_else(|| tree.unexposed_notice())
            {
                println!("disclosure rendered:\n{hint}");
            } else {
                println!("NOTHING ARRIVED AND NOTHING DISCLOSED — the blind spot");
            }
        }
        None => println!("no tree at all — nothing was published within {SETTLE:?}"),
    }
    println!("stop: {:?}", glass.stop());
}

fn run_probes(backend: &str, failures: &mut Vec<String>) {
    let Ok(list) = std::env::var(BROWSERS_VAR) else {
        println!("skipped: set {BROWSERS_VAR}=firefox,brave-browser");
        return;
    };
    let lever = Lever::from_env();
    for browser in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        probe(backend, browser, lever, failures);
    }
}

#[test]
#[ignore = "needs a browser and the a11y prerequisites; see the module doc"]
fn x11_browsers() {
    let mut failures = Vec::new();
    run_probes("x11", &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
#[ignore = "needs sway, a browser and the a11y prerequisites; see the module doc"]
fn wayland_browsers() {
    let mut failures = Vec::new();
    run_probes("wayland", &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
