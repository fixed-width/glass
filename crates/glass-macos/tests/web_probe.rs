//! Web-content PROBE for the macOS backend — prints what a browser publishes through
//! `AXUIElement`, with and without the candidate AX "enable" levers. Not a pass/fail mapping
//! test: it prints evidence and asserts nothing about what an engine ought to expose.
//!
//! ```sh
//! GLASS_WEB_PROBE_BROWSERS=/Applications/Safari.app GLASS_WEB_PROBE_LEVER=enhanced \
//!   cargo test -p glass-macos --test web_probe
//! ```
//!
//! `GLASS_WEB_PROBE_BROWSERS` is a comma-separated list of launch commands — `.app` bundle
//! paths or plain executables, exactly what `AppSpec::run[0]` accepts. Each is launched with
//! the `examples/web-role-fixture/index.html` page as `run[1]`; when the launch path drops
//! that argument (an app LaunchServices opens by bundle rather than by command line), the
//! probe navigates by typing the URL into the address bar instead, and says which route ran.
//!
//! `GLASS_WEB_PROBE_LEVER` picks the enable lever set on the *application* element after the
//! launch: `enhanced` → `AXEnhancedUserInterface`, `manual` → `AXManualAccessibility`,
//! anything else (including unset) → no lever, the baseline reading.
//!
//! With `GLASS_WEB_PROBE_BROWSERS` unset the probe reads the **side effect** of the lever
//! instead: it launches TextEdit and prints five snapshot wall-clocks before the lever and
//! five after, so the cost of leaving `AXEnhancedUserInterface` set on an ordinary AppKit app
//! is a measured number rather than a guess.
//!
//! Two readings need the AX API raw, so this file carries `unsafe` where the rest of the
//! tests do not (`#![allow(unsafe_code)]`, mirroring the crate-wide opt-in in `lib.rs`):
//! setting the lever is a write no reader exposes, and the pre-lever web-area reading has to
//! see elements `glass-a11y-macos`'s walk prunes — `should_skip` drops a zero-geometry
//! element, which is exactly the shape a lazily-built web area is expected to have before it
//! is disclosed.
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "web_probe"` entry): same
//! main-thread requirement as `a11y`/`role_probe` — `MacosPlatform::start_app` reaches
//! AppKit's `ffi::app_kit_init()`, which libtest's per-test worker threads cannot provide.
//!
//! Needs the Accessibility and Screen Recording TCC grants, so it only reads anything through
//! the granted-bundle run described in `tests/a11y.rs`'s module doc.

#![allow(unsafe_code)]

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
    use std::ptr::NonNull;
    use std::time::{Duration, Instant};

    use glass_a11y_macos::MacosA11y;
    use glass_core::{
        Accessibility, AppSpec, AxContext, AxNode, AxRole, AxTarget, AxTree, Deadline, GlassError,
        KeyEvent, MouseButton, Platform, PointerEvent, SandboxLevel, WalkLimits, WindowHint,
        WindowOp, role_histogram,
    };
    use glass_macos::MacosPlatform;
    use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
    use objc2_core_foundation::{
        CFArray, CFBoolean, CFRetained, CFString, CFType, CGPoint, CGSize, kCFBooleanTrue,
    };

    use crate::common::with_stop_app;

    const BROWSERS_VAR: &str = "GLASS_WEB_PROBE_BROWSERS";
    const LEVER_VAR: &str = "GLASS_WEB_PROBE_LEVER";

    /// The app the side-effect reading times when no browser is named — a plain AppKit app
    /// with a real tree, and one every Mac has.
    const TIMING_APP: &str = "/System/Applications/TextEdit.app";

    /// How long to keep re-reading the tree for the page's own elements before calling the
    /// content missing.
    const SETTLE: Duration = Duration::from_secs(8);

    /// Window-discovery budget. A cold browser launch is slower than the AppKit fixture's.
    const LAUNCH_TIMEOUT_MS: u64 = 30_000;

    /// After `start_app`, before anything is read: AppKit finishes building the tree behind a
    /// window a beat after the window appears (same settle `tests/a11y.rs` uses).
    const STARTUP_SETTLE: Duration = Duration::from_millis(1200);

    /// After an actuation, before the re-read that looks for its effect.
    const ACTION_SETTLE: Duration = Duration::from_millis(600);

    /// The page's `<h1>` — what "the page is readable" means for this probe.
    const HEADING: &str = "Glass web fixture";
    /// The fixture button's accessible name.
    const BUTTON: &str = "click me";
    /// The fixture text input's accessible name, from its `<label for>`.
    const INPUT: &str = "text input";
    /// What the result paragraph reads after the button fires.
    const CLICKED: &str = "clicked";
    /// What `set_value` writes into the text input.
    const TYPED: &str = "typed by glass";
    /// What the keyboard control types into the same field — a second route to it.
    const KEYED: &str = "keyed by glass";
    /// The `<iframe>`'s title, so the nested document is identifiable in the outline.
    const FRAME: &str = "nested document";

    fn page_url() -> String {
        format!(
            "file://{}/../../examples/web-role-fixture/index.html",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    /// The AX attribute a run sets on the application element to try to make the app publish
    /// more than it does by default.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Lever {
        None,
        Enhanced,
        Manual,
    }

    impl Lever {
        fn from_env() -> Self {
            match std::env::var(LEVER_VAR).unwrap_or_default().as_str() {
                "enhanced" => Lever::Enhanced,
                "manual" => Lever::Manual,
                _ => Lever::None,
            }
        }

        /// The attribute name, or `None` for the baseline run that sets nothing.
        fn attribute(self) -> Option<&'static str> {
            match self {
                Lever::None => None,
                Lever::Enhanced => Some(ENHANCED),
                Lever::Manual => Some(MANUAL),
            }
        }
    }

    /// The lever AppKit historically read as "an assistive client is attached".
    const ENHANCED: &str = "AXEnhancedUserInterface";
    /// The lever Electron/Chromium-family hosts read for the same purpose.
    const MANUAL: &str = "AXManualAccessibility";

    /// Set `attr` to true on `app`. Both levers are write-only switches an assistive client is
    /// expected to set on the app it reads — no reader API exposes them, so this is the one
    /// place the probe writes AX directly.
    fn set_lever(app: &AXUIElement, attr: &str) -> Result<(), String> {
        let name = CFString::from_str(attr);
        // `objc2-core-foundation` has no `CFBoolean` constructor; the two singletons are the
        // documented way to name a CF boolean, as `glass-macos::session`'s tests do.
        let value: &CFType = match unsafe { kCFBooleanTrue } {
            Some(v) => v,
            None => return Err("kCFBooleanTrue was null".to_string()),
        };
        // SAFETY: `app` is a live `AXUIElement`, `name` a valid `CFString`, and `value` a
        // valid `CFType` — matching `set_attribute_value`'s documented parameters, the same
        // call `glass-a11y-macos::ffi::set_string_value` makes.
        let err = unsafe { app.set_attribute_value(&name, value) };
        if err == AXError::Success {
            Ok(())
        } else {
            Err(format!("AXError {}", err.0))
        }
    }

    /// Every name in `el`'s `AXAttributeNames`, or an empty list on any failure.
    fn attribute_names(el: &AXUIElement) -> Vec<String> {
        let mut raw: *const CFArray = std::ptr::null();
        // SAFETY: `el` is a live `AXUIElement` and `raw` a valid local out-param slot matching
        // `AXUIElementCopyAttributeNames`'s documented signature.
        let err = unsafe { el.copy_attribute_names(NonNull::from(&mut raw)) };
        if err != AXError::Success {
            return Vec::new();
        }
        let Some(nn) = NonNull::new(raw.cast_mut()) else {
            return Vec::new();
        };
        // SAFETY: the Copy call returned an already-retained (+1) array per Core Foundation's
        // Copy/Create rule, and `AXAttributeNames` is documented to hold `CFString`s — the cast
        // only attaches compile-time element-type information.
        let names: CFRetained<CFArray<CFString>> =
            unsafe { CFRetained::cast_unchecked(CFRetained::from_raw(nn)) };
        names.iter().map(|n| n.to_string()).collect()
    }

    /// What the application element says about a lever before anything writes it: whether AX
    /// lists it at all, what it currently reads as, and whether AX calls it settable.
    ///
    /// Printed on every run because a refused write means three different things depending on
    /// this — an attribute the app never declares, one it declares read-only, and one it
    /// declares settable and then rejects are three different findings, and the `AXError` alone
    /// does not separate them.
    fn describe_lever(el: &AXUIElement, attr: &str) -> String {
        let listed = attribute_names(el).iter().any(|name| name == attr);
        let value = copy_attr(el, attr)
            .and_then(|v| v.downcast::<CFBoolean>().ok())
            .map(|b| b.value());
        let name = CFString::from_str(attr);
        let mut settable: u8 = 0;
        // SAFETY: `el` is a live `AXUIElement`, `name` a valid `CFString`, and `settable` a
        // valid local out-param matching the documented `Boolean *` parameter (mirrors
        // `glass-a11y-macos::ffi::is_settable`).
        let err = unsafe { el.is_attribute_settable(&name, NonNull::from(&mut settable)) };
        let settable = if err == AXError::Success {
            format!("{}", settable != 0)
        } else {
            format!("unreadable (AXError {})", err.0)
        };
        format!("listed={listed} value={value:?} settable={settable}")
    }

    /// Report both levers' state on `pid`'s application element, then set the one this run asked
    /// for.
    fn apply_lever(pid: i32, lever: Lever) {
        // SAFETY: `AXUIElementCreateApplication` never returns NULL per Apple's documented
        // contract (the binding `.expect()`s it), and `pid` is a plain process id with no
        // aliasing or lifetime preconditions — the shape `glass-a11y-macos::ffi::app_element`
        // uses.
        let app = unsafe { AXUIElement::new_application(pid) };
        for attr in [ENHANCED, MANUAL] {
            println!("lever {attr}: {}", describe_lever(&app, attr));
        }
        match lever.attribute() {
            Some(attr) => println!("set {attr}: {:?}", set_lever(&app, attr)),
            None => println!("set: nothing — this run is the baseline"),
        }
    }

    /// Copy one attribute as an owned CF value, or `None` for any failure or a null result.
    /// The probe's raw reads collapse absent and failed alike — every caller here prints what
    /// it got and moves on.
    fn copy_attr(el: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
        let attr = CFString::from_str(name);
        let mut raw: *const CFType = std::ptr::null();
        // SAFETY: `el` is a live `AXUIElement` and `raw` a valid local out-param slot,
        // matching `AXUIElementCopyAttributeValue`'s documented signature (mirrors
        // `glass-a11y-macos::ffi::copy_attribute_checked`).
        let err = unsafe { el.copy_attribute_value(&attr, NonNull::from(&mut raw)) };
        if err != AXError::Success {
            return None;
        }
        let nn = NonNull::new(raw.cast_mut())?;
        // SAFETY: the Copy call returned an already-retained (+1) value per Core Foundation's
        // Copy/Create rule, so taking ownership here releases it on drop without an extra
        // retain.
        Some(unsafe { CFRetained::from_raw(nn) })
    }

    fn attr_string(el: &AXUIElement, name: &str) -> Option<String> {
        copy_attr(el, name)?
            .downcast::<CFString>()
            .ok()
            .map(|s| s.to_string())
    }

    /// `AXPosition` as a point pair.
    fn attr_position(el: &AXUIElement) -> Option<(f64, f64)> {
        let value = copy_attr(el, "AXPosition")?.downcast::<AXValue>().ok()?;
        let mut point = CGPoint { x: 0.0, y: 0.0 };
        // SAFETY: `value` was downcast-verified to be a real `AXValue`, and `point` is a valid
        // local out-param whose type matches the requested `AXValueType::CGPoint`.
        let ok = unsafe { value.value(AXValueType::CGPoint, NonNull::from(&mut point).cast()) };
        ok.then_some((point.x, point.y))
    }

    /// `AXSize` as a width/height pair, in points.
    fn attr_size(el: &AXUIElement) -> Option<(f64, f64)> {
        let value = copy_attr(el, "AXSize")?.downcast::<AXValue>().ok()?;
        let mut size = CGSize {
            width: 0.0,
            height: 0.0,
        };
        // SAFETY: as in `attr_position`, with `AXValueType::CGSize`/`CGSize`.
        let ok = unsafe { value.value(AXValueType::CGSize, NonNull::from(&mut size).cast()) };
        ok.then_some((size.width, size.height))
    }

    fn ax_children(el: &AXUIElement) -> Vec<CFRetained<AXUIElement>> {
        let Some(value) = copy_attr(el, "AXChildren") else {
            return Vec::new();
        };
        let Ok(array) = value.downcast::<CFArray>() else {
            return Vec::new();
        };
        // SAFETY: `AXChildren` is documented to hold `AXUIElementRef`s; the cast only attaches
        // compile-time element-type information, with no runtime effect (the technique
        // `glass-a11y-macos::ffi::array_of_elements` uses).
        let typed: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(array) };
        typed.iter().collect()
    }

    /// Node and depth bounds for the raw walk. It descends where the reader would not, so it
    /// needs its own rails; both are generous for one browser window.
    const MAX_RAW_NODES: usize = 4000;
    const MAX_RAW_DEPTH: usize = 40;

    /// One element on the way to a web area: how it prints, and whether
    /// `glass-a11y-macos`'s `should_skip` would have pruned it.
    #[derive(Clone)]
    struct PathStep {
        label: String,
        skipped: bool,
    }

    /// One `AXWebArea` the raw walk found, with the ancestry that reached it.
    struct WebArea {
        /// The chain from the application element down to the web area, root first.
        path: Vec<PathStep>,
        position: Option<(f64, f64)>,
        size: Option<(f64, f64)>,
        children: usize,
    }

    /// What the raw walk saw, beside the web areas themselves.
    #[derive(Default)]
    struct RawScan {
        web_areas: Vec<WebArea>,
        scanned: usize,
        zero_area: usize,
        truncated: bool,
    }

    /// Walk `pid`'s application element with no pruning at all, collecting every `AXWebArea`.
    ///
    /// The reader cannot answer this: `glass-a11y-macos`'s `should_skip` drops any element
    /// whose `AXSize` has a non-positive dimension before descending into it, so a web area
    /// that exists but has not been laid out yet — and every element beneath it — is absent
    /// from the tree rather than present-and-childless. Only a walk that ignores geometry can
    /// tell "the engine never published a web area" from "the reader pruned the one it did".
    fn raw_scan(pid: i32) -> RawScan {
        // SAFETY: as in `set_lever` — `AXUIElementCreateApplication` never returns NULL and
        // takes a plain pid.
        let app = unsafe { AXUIElement::new_application(pid) };
        let mut scan = RawScan::default();
        let mut path = Vec::new();
        raw_walk(&app, 0, &mut path, &mut scan);
        scan
    }

    fn raw_walk(el: &AXUIElement, depth: usize, path: &mut Vec<PathStep>, scan: &mut RawScan) {
        if scan.scanned >= MAX_RAW_NODES || depth > MAX_RAW_DEPTH {
            scan.truncated = true;
            return;
        }
        scan.scanned += 1;
        let role = attr_string(el, "AXRole").unwrap_or_else(|| "(no AXRole)".to_string());
        let size = attr_size(el);
        // The reader's own predicate, restated: a readable non-positive dimension prunes; an
        // unreadable size keeps the element (`glass-a11y-macos::reader::should_skip`).
        let skipped = matches!(size, Some((w, h)) if w <= 0.0 || h <= 0.0);
        if skipped {
            scan.zero_area += 1;
        }
        let children = ax_children(el);
        path.push(PathStep {
            label: match size {
                Some((w, h)) => format!("{role}[{w:.0}x{h:.0}]"),
                None => format!("{role}[size unreadable]"),
            },
            skipped,
        });
        if role == "AXWebArea" {
            scan.web_areas.push(WebArea {
                path: path.clone(),
                position: attr_position(el),
                size,
                children: children.len(),
            });
        }
        for child in &children {
            raw_walk(child, depth + 1, path, scan);
        }
        path.pop();
    }

    /// Print a raw scan under `label`, saying for each web area whether the reader's
    /// zero-geometry prune would have reached it — at the web area itself, or at an ancestor.
    fn report_raw(label: &str, scan: &RawScan) {
        println!(
            "raw AX scan ({label}): {} elements walked, {} of them zero-area, truncated={}, \
             AXWebArea count={}",
            scan.scanned,
            scan.zero_area,
            scan.truncated,
            scan.web_areas.len()
        );
        for area in &scan.web_areas {
            let pruned = area
                .path
                .iter()
                .find(|step| step.skipped)
                .map(|step| step.label.as_str());
            let path: Vec<&str> = area.path.iter().map(|step| step.label.as_str()).collect();
            println!(
                "  AXWebArea position={:?} size={:?} children={} \
                 pruned_by_should_skip_at={pruned:?}\n    path: {}",
                area.position,
                area.size,
                area.children,
                path.join(" > ")
            );
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

    /// The first node carrying `text` as its whole name or value. Exact, not a substring: the
    /// fixture's result paragraph reads "not clicked" before the click, which contains
    /// "clicked".
    fn carries<'a>(tree: &'a AxTree, text: &str) -> Option<&'a AxNode> {
        find(tree, &|n| {
            n.name.as_deref() == Some(text) || n.value.as_deref() == Some(text)
        })
    }

    /// The fixture's text input. Matched by the label first; failing that, the first editable
    /// `TextField` **inside a `Document`**, which keeps the browser's own address bar — also an
    /// editable text field — out of the answer.
    fn text_input(tree: &AxTree) -> Option<&AxNode> {
        fn editable_field(node: &AxNode) -> Option<&AxNode> {
            if node.states.editable && node.role == AxRole::TextField {
                return Some(node);
            }
            node.children.iter().find_map(editable_field)
        }
        find(tree, &|n| {
            n.states.editable
                && (n.name.as_deref() == Some(INPUT) || n.description.as_deref() == Some(INPUT))
        })
        .or_else(|| documents(tree).into_iter().find_map(editable_field))
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

    fn target_of(node: &AxNode) -> AxTarget {
        AxTarget {
            id: node.id,
            role: node.role,
            name: node.name.clone(),
            bounds: node.bounds,
            value: node.value.clone(),
        }
    }

    /// One snapshot with node ids assigned, as every reader's caller takes it.
    fn snapshot(a11y: &mut MacosA11y, ctx: &AxContext) -> Result<AxTree, GlassError> {
        let mut tree = a11y.snapshot(ctx)?;
        tree.assign_ids();
        Ok(tree)
    }

    /// What proves the page's own content is in the tree, described — or `None` while it is
    /// not there.
    ///
    /// The heading has to be matched by role as well as name: the browser puts the page's
    /// title on its *window* too, so a name-only match reports the page as readable the
    /// instant it loads, whatever the engine published. The button is the second marker, for
    /// an engine that publishes the content without mapping `<h1>` to `AXHeading` — content
    /// that arrived under an unexpected role is still content that arrived.
    fn page_marker(tree: &AxTree) -> Option<String> {
        if let Some(node) = find(tree, &|n| {
            n.role == AxRole::Heading && n.name.as_deref() == Some(HEADING)
        }) {
            return Some(format!("Heading {HEADING:?} (#{})", node.id.0));
        }
        named(tree, BUTTON).map(|node| {
            format!(
                "Button {BUTTON:?} (#{}) — the page arrived, but its <h1> did not come back as \
                 a Heading",
                node.id.0
            )
        })
    }

    /// Re-snapshot until [`page_marker`] finds the page's own content or [`SETTLE`] elapses.
    /// Returns the last tree read, how long it took, and whether the content arrived. Each
    /// distinct error is printed once — a probe that stops at the first reads "nothing
    /// published" for a page that had not loaded yet.
    fn snapshot_until_page(
        a11y: &mut MacosA11y,
        ctx: &AxContext,
    ) -> (Option<AxTree>, Duration, bool) {
        let start = Instant::now();
        let mut last = None;
        let mut reported: Vec<String> = Vec::new();
        loop {
            match snapshot(a11y, ctx) {
                Ok(tree) => {
                    let marker = page_marker(&tree);
                    last = Some(tree);
                    if let Some(marker) = marker {
                        println!("page marker: {marker}");
                        return (last, start.elapsed(), true);
                    }
                }
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

    fn report_tree(tree: &AxTree) {
        println!("{} nodes, complete={}", tree.count, tree.is_complete());
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

    /// `Glass::click_element`'s actuation, at the seam this probe drives directly: the native
    /// AX action first, and a synthetic pointer click at the node's own bounds when the element
    /// exposes no action. Returns the label of the path that ran.
    fn click_like_glass(
        platform: &mut MacosPlatform,
        a11y: &mut MacosA11y,
        ctx: &AxContext,
        node: &AxNode,
    ) -> Result<String, String> {
        let target = target_of(node);
        match a11y.invoke(ctx, &target) {
            Ok(actuated) => return Ok(format!("native-action (actuated {actuated:?})")),
            Err(GlassError::AxUnsupported | GlassError::AxActionUnavailable(_)) => {}
            Err(e) => return Err(format!("invoke failed: {e}")),
        }
        let bounds = node.bounds.ok_or_else(|| {
            format!(
                "#{} has no bounds, so the pointer fallback has no point",
                node.id.0
            )
        })?;
        let (x, y) = bounds
            .clamped_center(ctx.window.width, ctx.window.height)
            .ok_or_else(|| format!("bounds {bounds:?} have zero area against {:?}", ctx.window))?;
        platform
            .send_pointer(&PointerEvent::Click {
                x,
                y,
                button: MouseButton::Left,
                count: 1,
                modifiers: vec![],
            })
            .map_err(|e| format!("pointer fallback at ({x},{y}) failed: {e}"))?;
        Ok(format!(
            "pointer at ({x},{y}) (element exposes no AX action)"
        ))
    }

    /// The actuation readings: click the fixture's button and check the page reacted, write the
    /// text input through `set_value` and read it back, then type into the same field by
    /// keyboard as the control that tells a failed write from a value this engine never reports.
    fn exercise(platform: &mut MacosPlatform, a11y: &mut MacosA11y, ctx: &AxContext) {
        let before = match snapshot(a11y, ctx) {
            Ok(tree) => tree,
            Err(e) => {
                println!("snapshot before the click failed: {e} — no click reading");
                return;
            }
        };
        match named(&before, BUTTON) {
            Some(button) => {
                println!(
                    "button: #{} role={:?} raw_role={} bounds={:?}",
                    button.id.0, button.role, button.raw_role, button.bounds
                );
                match click_like_glass(platform, a11y, ctx, button) {
                    Ok(method) => {
                        std::thread::sleep(ACTION_SETTLE);
                        match snapshot(a11y, ctx) {
                            Ok(after) => println!(
                                "click_element: {method} → result paragraph reads {CLICKED:?}: {}",
                                carries(&after, CLICKED).is_some()
                            ),
                            Err(e) => println!("click_element: {method} → re-snapshot failed: {e}"),
                        }
                    }
                    Err(e) => println!("click_element failed: {e}"),
                }
            }
            None => println!("no node named {BUTTON:?} — no click reading"),
        }

        let after = match snapshot(a11y, ctx) {
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
            "text input: #{} role={:?} raw_role={} name={:?} value={:?}",
            field.id.0, field.role, field.raw_role, field.name, field.value
        );
        let set = a11y.set_value(ctx, &target_of(field), TYPED);
        std::thread::sleep(ACTION_SETTLE);
        match snapshot(a11y, ctx) {
            Ok(after) => println!(
                "set_value: {set:?} → text input value={:?}",
                text_input(&after).and_then(|n| n.value.clone())
            ),
            Err(e) => println!("set_value: {set:?} → re-snapshot failed: {e}"),
        }

        // The control for the line above. An empty readback has two causes — the write never
        // landed, or this engine never reports a web input's text — and only text that reached
        // the field by another route tells them apart. The pointer and keyboard do not touch
        // the accessibility write path.
        let after = match snapshot(a11y, ctx) {
            Ok(tree) => tree,
            Err(e) => {
                println!("snapshot before the keyboard control failed: {e}");
                return;
            }
        };
        if let Some(field) = text_input(&after) {
            let focus = click_like_glass(platform, a11y, ctx, field);
            let keyed = platform.send_key(&KeyEvent::Text(KEYED.to_string()));
            std::thread::sleep(ACTION_SETTLE);
            match snapshot(a11y, ctx) {
                Ok(after) => println!(
                    "control — click {focus:?} then key {keyed:?} → text input value={:?}",
                    text_input(&after).and_then(|n| n.value.clone())
                ),
                Err(e) => println!("control — re-snapshot failed: {e}"),
            }
        }
    }

    /// Report the nested `<iframe>`: whether its own `Document` arrived, and what is inside it.
    fn report_iframe(tree: &AxTree) {
        let docs = documents(tree);
        println!("Document count: {}", docs.len());
        match named(tree, FRAME) {
            Some(node) => println!(
                "iframe: #{} role={:?} raw_role={} children={} bounds={:?}",
                node.id.0,
                node.role,
                node.raw_role,
                node.children.len(),
                node.bounds
            ),
            None => println!("no node named {FRAME:?} — the iframe did not arrive"),
        }
        println!(
            "iframe content: heading {:?}, button {:?}",
            carries(tree, "inside the frame").map(|n| (n.id.0, n.role)),
            named(tree, "frame button").map(|n| (n.id.0, n.role)),
        );
    }

    /// A window whose title carries `HEADING`, which is how the probe learns the page loaded
    /// without asking the accessibility tree — the very thing under test.
    fn page_titled_window(platform: &mut MacosPlatform) -> bool {
        match platform.list_windows() {
            Ok(windows) => {
                println!(
                    "windows: {:?}",
                    windows.iter().map(|w| w.title.clone()).collect::<Vec<_>>()
                );
                windows
                    .iter()
                    .any(|w| w.title.as_deref().is_some_and(|t| t.contains(HEADING)))
            }
            Err(e) => {
                println!("list_windows failed: {e}");
                false
            }
        }
    }

    /// Poll [`page_titled_window`] until the page's title appears or `budget` elapses.
    fn wait_for_page_title(platform: &mut MacosPlatform, budget: Duration) -> bool {
        let start = Instant::now();
        loop {
            if page_titled_window(platform) {
                return true;
            }
            if start.elapsed() > budget {
                return false;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Type `url` into the browser's address bar (⌘L, the URL, Return) — the fallback for a
    /// launch path that cannot carry the URL as an argument.
    fn navigate_by_address_bar(platform: &mut MacosPlatform, url: &str) -> Result<(), String> {
        platform
            .send_key(&KeyEvent::Chord("cmd+l".to_string()))
            .map_err(|e| format!("cmd+l: {e}"))?;
        std::thread::sleep(Duration::from_millis(400));
        platform
            .send_key(&KeyEvent::Text(url.to_string()))
            .map_err(|e| format!("typing the url: {e}"))?;
        std::thread::sleep(Duration::from_millis(400));
        platform
            .send_key(&KeyEvent::Chord("Return".to_string()))
            .map_err(|e| format!("Return: {e}"))
    }

    /// The pid the lever is set on and the raw walk starts from: the launched app's own
    /// process, which is the AX server for its whole window even when the engine renders in a
    /// child process.
    fn app_pid(platform: &MacosPlatform) -> Result<i32, String> {
        let pids = platform.app_pids();
        let pid = pids
            .first()
            .ok_or_else(|| "the backend reported no pid for the launched app".to_string())?;
        i32::try_from(*pid).map_err(|e| format!("pid {pid} does not fit an i32: {e}"))
    }

    fn context(platform: &mut MacosPlatform, fallback: &glass_core::WindowGeometry) -> AxContext {
        // Re-read rather than reuse the launch geometry: the window moves while the page loads,
        // and every bound in the tree — and so every pointer fallback — is relative to it.
        let window = platform
            .window(&WindowOp::Geometry)
            .unwrap_or_else(|_| fallback.clone());
        AxContext {
            pids: platform.app_pids(),
            window,
            window_handle: None,
            a11y_bus_addr: None,
            // Cap lifted: a browser's chrome alone spends the default budget, and a truncated
            // walk makes the disclosure hedge instead of naming a cause.
            limits: WalkLimits::from_max_nodes(Some(0)),
            deadline: Deadline::UNBOUNDED,
        }
    }

    fn browser_spec(browser: &str, url: &str) -> AppSpec {
        AppSpec {
            build: None,
            run: vec![browser.to_string(), url.to_string()],
            cwd: None,
            env: vec![],
            window_hint: Some(WindowHint {
                title: Some(HEADING.to_string()),
                class: None,
            }),
            timeout_ms: LAUNCH_TIMEOUT_MS,
            sandbox: SandboxLevel::Off,
            a11y: true,
        }
    }

    fn probe_browser(browser: &str, lever: Lever, url: &str) -> Result<(), String> {
        println!("\n=== macos / {browser} / lever={lever:?} ===");
        let spec = browser_spec(browser, url);
        println!("run: {:?}", spec.run);
        let mut platform =
            MacosPlatform::new().map_err(|e| format!("MacosPlatform::new(): {e}"))?;

        with_stop_app(&mut platform, browser, |platform| {
            let started = Instant::now();
            let geometry = platform
                .start_app(&spec)
                .map_err(|e| format!("start_app({browser}): {e}"))?;
            println!("window mapped after {:?}: {geometry:?}", started.elapsed());
            std::thread::sleep(STARTUP_SETTLE);

            let carried = wait_for_page_title(platform, Duration::from_secs(6));
            if carried {
                println!("url delivery: the launch carried AppSpec::run[1] to the browser");
            } else {
                println!(
                    "url delivery: the launch did not carry AppSpec::run[1] — navigating by \
                     address bar"
                );
                navigate_by_address_bar(platform, url)?;
                let loaded = wait_for_page_title(platform, Duration::from_secs(10));
                println!("address-bar navigation reached the page: {loaded}");
            }

            let pid = app_pid(platform)?;
            // Three raw walks bracket the reads, because a web area that is built lazily and
            // one that is never built look identical from a single scan. This first one runs
            // before anything has snapshotted the app, so it says what the engine published on
            // its own once the page finished loading.
            report_raw("before any snapshot", &raw_scan(pid));

            let ctx = context(platform, &geometry);
            let mut a11y = MacosA11y::new();
            // The first read a caller would take.
            match snapshot(&mut a11y, &ctx) {
                Ok(tree) => println!(
                    "first snapshot after load: {} nodes, {} Document(s), marker {:?}",
                    tree.count,
                    documents(&tree).len(),
                    page_marker(&tree)
                ),
                Err(e) => println!("first snapshot after load failed: {e}"),
            }
            report_raw("after the first snapshot, before the lever", &raw_scan(pid));

            apply_lever(pid, lever);
            std::thread::sleep(ACTION_SETTLE);

            let (tree, settle, arrived) = snapshot_until_page(&mut a11y, &ctx);
            println!("page content arrived: {arrived} after {settle:?}");
            report_raw("after the lever", &raw_scan(pid));

            match tree {
                Some(tree) => {
                    report_tree(&tree);
                    report_iframe(&tree);
                    if arrived {
                        exercise(platform, &mut a11y, &ctx);
                        if let Ok(after) = snapshot(&mut a11y, &ctx) {
                            println!("--- tree after actuation ---");
                            report_tree(&after);
                        }
                    } else if let Some(hint) = tree.document_guidance() {
                        println!("disclosure rendered:\n{hint}");
                    } else {
                        println!("NO DOCUMENT AND NO DISCLOSURE — the blind spot");
                    }
                }
                None => println!("no tree at all — nothing was published within {SETTLE:?}"),
            }
            Ok(())
        })
    }

    /// How many snapshots each side of the side-effect reading takes.
    const TIMING_REPEATS: usize = 5;

    /// Five snapshot wall-clocks, printed individually beside the mean so one slow outlier is
    /// visible rather than averaged away.
    fn time_snapshots(label: &str, a11y: &mut MacosA11y, ctx: &AxContext) {
        let mut samples = Vec::with_capacity(TIMING_REPEATS);
        for repeat in 0..TIMING_REPEATS {
            let started = Instant::now();
            match a11y.snapshot(ctx) {
                Ok(tree) => {
                    // Inside the timed window, so no future laziness can move walk work out
                    // from under the timer.
                    std::hint::black_box(&tree);
                    samples.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                Err(e) => {
                    println!("  {label}: repeat {repeat} failed: {e}");
                    break;
                }
            }
        }
        let rendered: Vec<String> = samples.iter().map(|ms| format!("{ms:.0}ms")).collect();
        let mean = if samples.is_empty() {
            f64::NAN
        } else {
            samples.iter().sum::<f64>() / samples.len() as f64
        };
        println!("  {label}: mean {mean:.0}ms over {rendered:?}");
    }

    /// The side-effect reading: what leaving a lever set costs an ordinary AppKit app. Both
    /// halves run against one launch, so the difference between them is the lever and not two
    /// different app startups.
    fn probe_timing(lever: Lever) -> Result<(), String> {
        println!("\n=== macos / {TIMING_APP} / lever={lever:?} (side-effect reading) ===");
        let spec = AppSpec {
            build: None,
            run: vec![TIMING_APP.to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: LAUNCH_TIMEOUT_MS,
            sandbox: SandboxLevel::Off,
            a11y: false,
        };
        let mut platform =
            MacosPlatform::new().map_err(|e| format!("MacosPlatform::new(): {e}"))?;

        with_stop_app(&mut platform, TIMING_APP, |platform| {
            let geometry = platform
                .start_app(&spec)
                .map_err(|e| format!("start_app({TIMING_APP}): {e}"))?;
            println!("started {TIMING_APP}: {geometry:?}");
            std::thread::sleep(STARTUP_SETTLE);

            let ctx = context(platform, &geometry);
            let mut a11y = MacosA11y::new();
            if let Ok(tree) = snapshot(&mut a11y, &ctx) {
                println!("tree before the lever: {} nodes", tree.count);
            }
            time_snapshots("before the lever", &mut a11y, &ctx);

            let pid = app_pid(platform)?;
            // With no lever, the block below is the measurement's own noise — the control for a
            // run where one was actually set.
            apply_lever(pid, lever);
            std::thread::sleep(ACTION_SETTLE);

            if let Ok(tree) = snapshot(&mut a11y, &ctx) {
                println!("tree after the lever: {} nodes", tree.count);
            }
            time_snapshots("after the lever", &mut a11y, &ctx);
            Ok(())
        })
    }

    pub(super) fn run() {
        let lever = Lever::from_env();
        let url = page_url();
        println!("lever: {lever:?}");
        println!("page: {url}");

        let mut failures = Vec::new();
        match std::env::var(BROWSERS_VAR) {
            Ok(list) if !list.trim().is_empty() => {
                for browser in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if let Err(e) = probe_browser(browser, lever, &url) {
                        failures.push(e);
                    }
                }
            }
            _ => {
                println!(
                    "{BROWSERS_VAR} unset — reading the lever's side effect on {TIMING_APP} \
                     instead"
                );
                if let Err(e) = probe_timing(lever) {
                    failures.push(e);
                }
            }
        }

        if failures.is_empty() {
            println!("\nWEB_PROBE_DONE");
            std::process::exit(0);
        }
        // Printed, not `fail`ed through stderr alone: the evidence above is the point, and a
        // launch that never happened is the one thing worth a non-zero exit.
        println!("\nWEB_PROBE_FAILURES:\n{}", failures.join("\n"));
        crate::common::fail(failures.join("\n"));
    }
}
