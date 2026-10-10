//! Opt-in read-only native lifecycle checks, run on the true main thread.
//! Requires an existing Accessibility grant and GLASS_WINDOW_TARGET_FIXTURE_BIN.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("skipped (not macOS): read-only target lifecycle");
}

#[cfg(target_os = "macos")]
fn main() {
    if !std::env::args().any(|arg| arg == "--ignored" || arg == "--include-ignored") {
        println!("skipped: read-only target lifecycle requires --ignored");
        return;
    }
    native::run();
}

#[cfg(target_os = "macos")]
mod native {
    use glass_core::{AppSpec, Deadline, GlassError, SandboxLevel};
    use glass_macos::window_directed::{OwnedTarget, TargetObservation};
    use std::time::{Duration, Instant};

    fn launch(scenario: &str) -> OwnedTarget {
        let fixture = std::env::var("GLASS_WINDOW_TARGET_FIXTURE_BIN")
            .expect("build fixture/window_target.swift and set GLASS_WINDOW_TARGET_FIXTURE_BIN");
        let spec = AppSpec {
            build: None,
            run: vec![fixture, scenario.into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 3_000,
            sandbox: SandboxLevel::Default,
            a11y: true,
        };
        OwnedTarget::launch(&spec, &[]).expect("launch fresh owned fixture")
    }

    fn first(target: &mut OwnedTarget) -> Result<TargetObservation, GlassError> {
        std::thread::sleep(Duration::from_millis(500));
        let deadline = Deadline::from_millis(3_000);
        loop {
            match target.observe(deadline) {
                Err(error)
                    if matches!(error.cause(), GlassError::WindowNotFound)
                        && !deadline.has_passed() =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                result => return result,
            }
        }
    }

    fn remains_unqualified(observation: &TargetObservation) {
        assert!(!observation.window_lifetime_verified);
        assert!(!observation.configuration_verified);
        assert!(!observation.pixel_geometry_verified);
        assert!(!observation.input_admitted);
    }

    fn changed(target: &mut OwnedTarget, reason: &str) {
        let end = Instant::now() + Duration::from_secs(8);
        loop {
            match target.observe(Deadline::from_millis(1_000)) {
                Ok(observation) => remains_unqualified(&observation),
                Err(error) => {
                    assert!(error.to_string().contains(reason), "{error}");
                    println!("expected lifecycle refusal: {error}");
                    assert!(
                        target
                            .observe(Deadline::from_millis(1_000))
                            .unwrap_err()
                            .to_string()
                            .contains("invalidated")
                    );
                    break;
                }
            }
            assert!(Instant::now() < end, "fixture did not change");
            std::thread::sleep(Duration::from_millis(40));
        }
        target.stop().expect("confirm original child cleanup");
    }

    pub fn run() {
        let mut stable = launch("stable");
        let observation = first(&mut stable).expect("read one owned window");
        println!("{observation:#?}");
        remains_unqualified(&observation);
        assert!(observation.controls.iter().any(|node| {
            node.role == "AXButton"
                && node.identifier.as_deref() == Some("observed-button")
                && node.parent.is_some()
                && node.enabled == Some(true)
        }));
        let refreshed = stable
            .observe(Deadline::from_millis(1_000))
            .expect("refresh original window");
        assert_eq!(refreshed.process, observation.process);
        assert_eq!(refreshed.window_id, observation.window_id);
        remains_unqualified(&refreshed);
        stable.stop().unwrap();
        stable.stop().unwrap();
        assert!(stable.observe(Deadline::from_millis(1_000)).is_err());
        println!("stable refresh and idempotent stop passed");

        let mut replacement = launch("replace");
        first(&mut replacement).expect("read initial replacement-test window");
        changed(&mut replacement, "replaced");

        let mut competing = launch("compete");
        let error = first(&mut competing).unwrap_err();
        assert!(
            error.to_string().contains("competing AX windows"),
            "{error}"
        );
        competing.stop().unwrap();
        println!("competing windows refused");

        let mut exited = launch("exit");
        first(&mut exited).expect("read initial exit-test window");
        std::thread::sleep(Duration::from_secs(6));
        let error = exited.observe(Deadline::from_millis(1_000)).unwrap_err();
        assert!(error.to_string().contains("owned child exited"), "{error}");
        assert!(
            exited
                .observe(Deadline::from_millis(1_000))
                .unwrap_err()
                .to_string()
                .contains("invalidated")
        );
        exited.stop().unwrap();
        println!("exited child refused without adoption");
        println!("read-only lifecycle checks passed; input remains unadmitted");
    }
}
