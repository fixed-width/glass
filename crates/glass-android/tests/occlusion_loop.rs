//! Live window-occlusion regression. Requires a booted AVD, the updated accessibility
//! companion, and GLASS_ANDROID_ROLE_FIXTURE_APK built from examples/android-role-fixture.

use glass_android::{
    A11yServiceRegistry, AgentRegistry, AndroidPlatform, EmulatorRegistry, ServiceA11y,
};
use glass_core::accessibility::{AxNode, AxRole, AxTree};
use glass_core::{
    ActionMode, ActionTarget, ActionabilityCheckName, ActionabilityVerdict, AppSpec, Backend,
    BaselineStore, ClickTargetParams, Deadline, DispatchStatus, Glass, KeyEvent, Platform,
    PointerEvent, SandboxLevel, SemanticActionFailureKind, SemanticSelector, SemanticTarget,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;

struct CountingAndroidPlatform {
    inner: AndroidPlatform,
    pointer_events: Arc<Mutex<Vec<PointerEvent>>>,
    key_events: Arc<Mutex<Vec<KeyEvent>>>,
}

impl Platform for CountingAndroidPlatform {
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

fn click(name: &str) -> ClickTargetParams {
    ClickTargetParams {
        target: ActionTarget::Semantic(SemanticTarget {
            target: SemanticSelector::new(Some(name.into()), Some(AxRole::Button), vec![]).unwrap(),
            within: None,
        }),
        mode: ActionMode::Pointer,
        timeout_ms: Some(3_000),
        max_nodes: None,
    }
}

fn find<'a>(node: &'a AxNode, name: &str) -> Option<&'a AxNode> {
    if node.name.as_deref() == Some(name) || node.description.as_deref() == Some(name) {
        return Some(node);
    }
    node.children.iter().find_map(|child| find(child, name))
}

fn counters(tree: &AxTree) -> Option<&str> {
    find(&tree.root, "Occlusion counters").and_then(|node| node.name.as_deref())
}

fn await_counters(glass: &mut Glass, expected: &str) -> AxTree {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let tree = glass.a11y_snapshot(None).expect("read counters");
        if counters(&tree) == Some(expected) {
            return tree;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected}: {}",
            tree.to_outline()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "requires Android AVD, updated GLASS_ANDROID_A11Y_APK and GLASS_ANDROID_ROLE_FIXTURE_APK"]
fn covering_windows_refuse_before_dispatch_and_pass_through_windows_allow_taps() {
    let companion = std::env::var("GLASS_ANDROID_A11Y_APK").expect("companion APK");
    let fixture = std::env::var("GLASS_ANDROID_ROLE_FIXTURE_APK").expect("role fixture APK");
    for (activity, covered) in [
        ("OcclusionWindow", true),
        ("OcclusionWindowNarrow", true),
        ("OcclusionWindowPass", false),
    ] {
        let agents = AgentRegistry::new();
        let _stop_agent = common::StopAgent(&agents);
        let platform =
            AndroidPlatform::from_env(&EmulatorRegistry::new(), &agents).expect("attach");
        let adb = platform.resolved_adb();
        adb.run(["install", "-r", &fixture])
            .expect("install fixture");
        let _cleanup = common::OnDrop(|| {
            let _ = adb.run([
                "shell",
                "am",
                "force-stop",
                "tech.fixedwidth.glassrolefixture",
            ]);
            let _ = adb.run(["uninstall", "tech.fixedwidth.glassrolefixture"]);
        });
        let services = A11yServiceRegistry::new();
        let _restore_service = common::RestoreServiceState(&services);
        let client = services.ensure(&adb, &companion).expect("companion ready");
        let pointers = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let mut backend = Some(Backend {
            platform: Box::new(CountingAndroidPlatform {
                inner: platform,
                pointer_events: Arc::clone(&pointers),
                key_events: Arc::clone(&keys),
            }),
            accessibility: Some(Box::new(ServiceA11y::new(
                client,
                "tech.fixedwidth.glassrolefixture".into(),
            ))),
        });
        let factory: glass_core::PlatformFactory = Box::new(move |_| {
            backend.take().ok_or_else(|| {
                glass_core::GlassError::Backend("backend already constructed".into())
            })
        });
        let baselines =
            std::env::temp_dir().join(format!("glass-android-occlusion-{}", std::process::id()));
        let mut glass = Glass::new(factory, "android".into(), BaselineStore::new(baselines), 64);
        glass
            .start(&AppSpec {
                build: None,
                run: vec![format!("tech.fixedwidth.glassrolefixture/.{activity}")],
                cwd: None,
                env: vec![],
                window_hint: None,
                timeout_ms: 15_000,
                sandbox: SandboxLevel::Off,
                a11y: true,
            })
            .expect("launch fixture");
        await_counters(&mut glass, "target=0 cover=0 positive=0");
        // Window publication follows the activity's first layout.
        std::thread::sleep(Duration::from_millis(350));
        let window = glass
            .list_windows()
            .expect("fixture windows")
            .into_iter()
            .max_by_key(|window| {
                u64::from(window.geometry.width) * u64::from(window.geometry.height)
            })
            .expect("main fixture window");
        glass
            .select_window(window.id)
            .expect("select the activity, not its cover panel");
        let result = glass.click_target(&click("Covered action"));
        std::thread::sleep(Duration::from_millis(350));
        let after = glass
            .a11y_snapshot(None)
            .expect("read actual tap recipient");
        if covered {
            let refusal = match result {
                Err(error) => error,
                Ok(outcome) => panic!(
                    "{activity}: dispatched through cover: {outcome:?}; counters={:?}",
                    counters(&after)
                ),
            };
            assert_eq!(refusal.kind, SemanticActionFailureKind::NotActionable);
            assert_eq!(refusal.action_dispatch, DispatchStatus::NotDispatched);
            assert_eq!(
                refusal
                    .actionability
                    .checks
                    .iter()
                    .find(|c| c.name == ActionabilityCheckName::NonOccluded)
                    .unwrap()
                    .verdict,
                ActionabilityVerdict::Failed
            );
            assert_eq!(counters(&after), Some("target=0 cover=0 positive=0"));
            assert!(pointers.lock().unwrap().is_empty());
        } else {
            let outcome = result.expect("pass-through window must not reject the target");
            assert_eq!(outcome.action.dispatch, DispatchStatus::Dispatched);
            assert_eq!(
                outcome
                    .actionability
                    .checks
                    .iter()
                    .find(|c| c.name == ActionabilityCheckName::NonOccluded)
                    .unwrap()
                    .verdict,
                ActionabilityVerdict::Unproven
            );
            assert_eq!(counters(&after), Some("target=1 cover=0 positive=0"));
            assert_eq!(pointers.lock().unwrap().len(), 1);
        }
        let initial_taps = pointers.lock().unwrap().len();
        for (name, expected) in [
            (
                "Positive action",
                if covered {
                    "target=0 cover=0 positive=1"
                } else {
                    "target=1 cover=0 positive=1"
                },
            ),
            (
                "Covered action",
                if covered {
                    "target=1 cover=0 positive=1"
                } else {
                    "target=2 cover=0 positive=1"
                },
            ),
        ] {
            let outcome = glass.click_target(&click(name)).expect("uncovered action");
            assert_eq!(outcome.action.dispatch, DispatchStatus::Dispatched);
            assert_eq!(
                outcome
                    .actionability
                    .checks
                    .iter()
                    .find(|c| c.name == ActionabilityCheckName::NonOccluded)
                    .unwrap()
                    .verdict,
                ActionabilityVerdict::Unproven
            );
            await_counters(&mut glass, expected);
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(
                counters(&glass.a11y_snapshot(None).unwrap()),
                Some(expected)
            );
        }
        assert_eq!(pointers.lock().unwrap().len(), initial_taps + 2);
        assert!(keys.lock().unwrap().is_empty());
        glass.stop().expect("stop fixture");
        println!("ANDROID_WINDOW_OCCLUSION_PASS {activity}");
    }
}
