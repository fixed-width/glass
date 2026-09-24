//! Run on Windows with an interactive desktop: cargo test -p glass-a11y-windows --release -- --ignored cache_tests --test-threads=1
use super::*;
use glass_core::{AxRole, TruncationLimit, WalkLimits, WindowGeometry};
use std::process::{Child, Command, Stdio};

struct Fixture {
    child: Child,
    _directory: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Fixture {
    fn start(winforms: bool) -> (Self, AxContext) {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/cache.ps1"
            ))
            .arg("-ReadyFile")
            .arg(&ready);
        if winforms {
            command.arg("-WinForms");
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Self {
            child,
            _directory: directory,
        };
        let end = Instant::now() + Duration::from_secs(15);
        let handle = loop {
            if let Ok(text) = std::fs::read_to_string(&ready)
                && let Ok(handle) = text.parse::<i64>()
            {
                break handle;
            }
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "fixture exited before publishing its window"
            );
            assert!(Instant::now() < end, "fixture did not publish its window");
            std::thread::sleep(Duration::from_millis(50));
        };
        let automation = UIAutomation::new().unwrap();
        let window = automation
            .element_from_handle(Handle::from(handle as isize))
            .unwrap();
        let bounds = window.get_bounding_rectangle().unwrap();
        let ctx = AxContext {
            pids: vec![fixture.child.id()],
            window: WindowGeometry {
                x: bounds.get_left(),
                y: bounds.get_top(),
                width: bounds.get_width() as u32,
                height: bounds.get_height() as u32,
            },
            window_handle: Some(handle),
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: Deadline::UNBOUNDED,
        };
        (fixture, ctx)
    }
}

fn named<'a>(tree: &'a AxTree, name: &str) -> &'a AxNode {
    tree.find_first(|node| node.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn target(node: &AxNode) -> AxTarget {
    AxTarget {
        id: node.id,
        role: node.role,
        name: node.name.clone(),
        bounds: node.bounds,
        value: node.value.clone(),
    }
}

fn assert_live_matches(ctx: &AxContext, tree: &AxTree) {
    let automation = UIAutomation::new().unwrap();
    let walker = automation.get_control_view_walker().unwrap();
    let root = find_app_window(&automation, ctx).unwrap();
    for id in 0..tree.count {
        let node = tree.find(AxNodeId(id as u32)).unwrap();
        let el = find_target(&walker, &root, ctx, &target(node)).unwrap();
        let ct = verify_target_fingerprint(&el, ctx, &target(node)).unwrap();
        let (mut facts, value) = gather(&el, ct, ctx.deadline, ReadMode::Current).unwrap();
        facts.offscreen |= bounds_are_offscreen(node.bounds, (ctx.window.width, ctx.window.height));
        assert_eq!(
            (node.states, &node.value),
            (crate::mapping::map_states(&facts), &value),
            "node {id}"
        );
        assert_eq!(
            node.description,
            normalize_description(&el.get_help_text().unwrap(), node.name.as_deref()),
            "node {id}"
        );
    }
}

#[test]
#[ignore = "needs an interactive Windows desktop and WPF"]
fn cached_states_values_and_ids_match_live_reads_and_actions() {
    let (_fixture, ctx) = Fixture::start(false);
    let mut reader = WindowsA11y::new();
    let tree = reader.snapshot(&ctx).unwrap();
    assert_eq!(tree.unreadable, 0);
    assert!(tree.truncated.is_none());
    assert_live_matches(&ctx, &tree);
    assert!(named(&tree, "Checked").states.checked);
    assert_eq!(named(&tree, "Toggle").role, AxRole::ToggleButton);
    assert!(named(&tree, "Toggle").states.checked);
    assert_eq!(named(&tree, "Slider").value.as_deref(), Some("42"));
    assert_eq!(named(&tree, "Progress").value.as_deref(), Some("63"));
    assert!(!named(&tree, "Read only").states.editable);
    assert!(!named(&tree, "Disabled").states.enabled);
    assert!(!named(&tree, "Button 0").states.checkable);
    assert!(named(&tree, "Selected item").states.selected);
    assert!(named(&tree, "Level 0").states.expanded);
    assert_eq!(
        named(&tree, "Note").description.as_deref(),
        Some("Extra help")
    );

    reader
        .set_value(&ctx, &target(named(&tree, "Note")), "updated")
        .unwrap();
    reader
        .invoke(&ctx, &target(named(&tree, "Checked")))
        .unwrap();
    let updated = reader.snapshot(&ctx).unwrap();
    assert_eq!(named(&updated, "Note").value.as_deref(), Some("updated"));
    assert!(!named(&updated, "Checked").states.checked);
}

#[test]
#[ignore = "needs an interactive Windows desktop and WPF"]
fn cache_preserves_node_depth_sibling_limits_and_live_rewalk() {
    let (_fixture, mut ctx) = Fixture::start(false);
    let mut reader = WindowsA11y::new();
    let complete = reader.snapshot(&ctx).unwrap();
    let count = complete.count;
    assert!(count > 24);
    ctx.limits.nodes = count;
    assert_eq!(
        reader.snapshot(&ctx).unwrap(),
        complete,
        "exactly at cap must be complete"
    );
    for (limits, expected) in [
        (
            WalkLimits {
                nodes: 1,
                ..WalkLimits::DEFAULT
            },
            TruncationLimit::Nodes,
        ),
        (
            WalkLimits {
                nodes: count - 1,
                ..WalkLimits::DEFAULT
            },
            TruncationLimit::Nodes,
        ),
        (
            WalkLimits {
                depth: 0,
                ..WalkLimits::DEFAULT
            },
            TruncationLimit::Depth,
        ),
        (
            WalkLimits {
                depth: 3,
                ..WalkLimits::DEFAULT
            },
            TruncationLimit::Depth,
        ),
        (
            WalkLimits {
                siblings: 1,
                ..WalkLimits::DEFAULT
            },
            TruncationLimit::Siblings,
        ),
    ] {
        ctx.limits = limits;
        let tree = reader.snapshot(&ctx).unwrap();
        assert_eq!(tree.truncated.unwrap().limit, expected);
        assert_eq!(tree.unreadable, 0);
        assert!(tree.count <= limits.nodes);
        assert_live_matches(&ctx, &tree);
    }
}

#[test]
fn snapshot_cache_is_element_scoped() {
    let automation = UIAutomation::new().unwrap();
    let cache = snapshot_cache(&automation, Deadline::UNBOUNDED).unwrap();
    assert_eq!(cache.get_tree_scope().unwrap(), TreeScope::Element);
}

#[test]
#[ignore = "needs an interactive Windows desktop and WinForms"]
fn cached_winforms_snapshot_matches_live_provider_reads() {
    let (_fixture, ctx) = Fixture::start(true);
    let mut reader = WindowsA11y::new();
    let tree = reader.snapshot(&ctx).unwrap();
    assert_eq!(tree.unreadable, 0);
    assert!(tree.truncated.is_none());
    assert_live_matches(&ctx, &tree);
    assert!(named(&tree, "Control 0").states.checked);
    reader
        .set_value(&ctx, &target(named(&tree, "Control 1")), "updated")
        .unwrap();
    let updated = reader.snapshot(&ctx).unwrap();
    assert_eq!(
        named(&updated, "Control 1").value.as_deref(),
        Some("updated")
    );
}
