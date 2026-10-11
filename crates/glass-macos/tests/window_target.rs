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
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    const SCHEDULED_CHANGE_WAIT: Duration = Duration::from_secs(6);

    fn launch(scenario: &str) -> OwnedTarget {
        let fixture = std::env::var("GLASS_WINDOW_TARGET_FIXTURE_BIN")
            .expect("build fixture/window_target.swift and set GLASS_WINDOW_TARGET_FIXTURE_BIN");
        launch_path(Path::new(&fixture), scenario)
    }

    fn launch_path(fixture: &Path, scenario: &str) -> OwnedTarget {
        OwnedTarget::launch(&fixture_spec(fixture, scenario), &[])
            .expect("launch fresh owned fixture")
    }

    fn fixture_spec(fixture: &Path, scenario: &str) -> AppSpec {
        AppSpec {
            build: None,
            run: vec![fixture.to_str().unwrap().into(), scenario.into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 3_000,
            sandbox: SandboxLevel::Default,
            a11y: true,
        }
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

    fn observed_owner(observation: &TargetObservation) {
        assert!(observation.window_identity_revalidated);
        assert!(observation.process.pid > 0);
        assert_ne!(observation.process.executable_uuid, [0; 16]);
    }

    fn changed(target: &mut OwnedTarget, reason: &str) {
        let end = Instant::now() + Duration::from_secs(8);
        loop {
            match target.observe(Deadline::from_millis(1_000)) {
                Ok(observation) => observed_owner(&observation),
                Err(error) => {
                    assert!(error.to_string().contains(reason), "{error}");
                    assert_eq!(
                        error.bound_dispatch(),
                        Some(glass_core::BoundDispatch::MayHaveDispatched)
                    );
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
        observed_owner(&observation);
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
        assert_eq!(refreshed.owner_connection, observation.owner_connection);
        assert_eq!(refreshed.owner_psn, observation.owner_psn);
        observed_owner(&refreshed);
        stable.stop().unwrap();
        stable.stop().unwrap();
        assert!(stable.observe(Deadline::from_millis(1_000)).is_err());
        println!("stable refresh and idempotent stop passed");

        let mut replacement = launch("replace");
        first(&mut replacement).expect("read initial replacement-test window");
        std::thread::sleep(SCHEDULED_CHANGE_WAIT);
        changed(&mut replacement, "replaced");

        let mut closed = launch("close");
        first(&mut closed).expect("read initial close-test window");
        std::thread::sleep(SCHEDULED_CHANGE_WAIT);
        changed(&mut closed, "window not found");

        let mut moved = launch("move");
        let original = first(&mut moved).expect("read initial move-test window");
        std::thread::sleep(SCHEDULED_CHANGE_WAIT);
        let refreshed = moved
            .observe(Deadline::from_millis(1_000))
            .expect("refresh moved original window");
        assert_eq!(refreshed.process, original.process);
        assert_eq!(refreshed.window_id, original.window_id);
        assert_eq!(refreshed.owner_connection, original.owner_connection);
        assert_eq!(refreshed.owner_psn, original.owner_psn);
        assert_ne!(refreshed.global_frame_points, original.global_frame_points);
        observed_owner(&refreshed);
        moved.stop().unwrap();
        println!("same-window geometry refresh passed");

        let mut competing = launch("compete");
        let error = first(&mut competing).unwrap_err();
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert!(
            error.to_string().contains("competing AX windows"),
            "{error}"
        );
        competing.stop().unwrap();
        println!("competing windows refused");

        let mut exited = launch("exit");
        first(&mut exited).expect("read initial exit-test window");
        std::thread::sleep(SCHEDULED_CHANGE_WAIT);
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
        development_changes();
        println!("read-only lifecycle and development changes passed; no input posted");
    }

    fn development_changes() {
        let original = std::env::var("GLASS_WINDOW_TARGET_FIXTURE_BIN").unwrap();
        let changed = std::env::args()
            .filter(|arg| arg != "--ignored" && arg != "--include-ignored")
            .nth(1)
            .expect("supply an independently changed app binary after --ignored");
        let mut expired = fixture_spec(Path::new(&original), "stable");
        expired.timeout_ms = 0;
        let error = OwnedTarget::launch(&expired, &[])
            .err()
            .expect("expired launch refused");
        assert_eq!(error.bound(), Some(glass_core::BoundKind::NotStarted));
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
        println!("expired launch refused before dispatch");

        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let moved_project = directory.path().join("moved-project");
        fs::create_dir(&project).unwrap();
        let app = project.join("app");
        fs::copy(&original, &app).unwrap();
        let mut spec = fixture_spec(Path::new("./app"), "development");
        spec.run.push("extra".into());
        spec.cwd = Some(project.clone());
        spec.env = vec![
            ("DEVELOPMENT_MARKER".into(), "custom".into()),
            ("DYLD_LIBRARY_PATH".into(), project.to_str().unwrap().into()),
        ];
        let mut target = OwnedTarget::launch(&spec, &[]).unwrap();
        let first_build = first(&mut target).expect("inspect an arbitrary development launch");
        observed_owner(&first_build);
        let replacement = project.join("replacement");
        fs::copy(&changed, &replacement).unwrap();
        fs::rename(&replacement, &app).unwrap();
        let observed = target.observe(Deadline::from_millis(1_000)).unwrap();
        assert_eq!(observed.process, first_build.process);
        assert_eq!(observed.window_id, first_build.window_id);
        fs::rename(&project, &moved_project).unwrap();
        let observed = target.observe(Deadline::from_millis(1_000)).unwrap();
        assert_eq!(observed.process, first_build.process);
        assert_eq!(observed.window_id, first_build.window_id);
        target.stop().unwrap();
        target.stop().unwrap();
        println!("executable replacement and cwd rename preserve the live owned process");

        spec.cwd = Some(moved_project.clone());
        spec.env[1].1 = moved_project.to_str().unwrap().into();
        let mut next = OwnedTarget::launch(&spec, &[]).unwrap();
        let next_build =
            first(&mut next).expect("inspect the rebuilt application without enrollment");
        observed_owner(&next_build);
        assert_ne!(next_build.process.lifetime, first_build.process.lifetime);
        assert_ne!(
            next_build.process.executable_uuid,
            first_build.process.executable_uuid
        );
        let expected = format!(
            "changed-custom-development-extra-{}",
            fs::canonicalize(&moved_project).unwrap().display()
        );
        assert!(
            next_build
                .controls
                .iter()
                .any(|node| node.identifier.as_deref() == Some(expected.as_str())),
            "changed app did not receive caller argv/cwd/env"
        );
        fs::remove_file(moved_project.join("app")).unwrap();
        let observed = next.observe(Deadline::from_millis(1_000)).unwrap();
        assert_eq!(observed.process, next_build.process);
        assert_eq!(observed.window_id, next_build.window_id);
        next.stop().unwrap();
        next.stop().unwrap();
        println!(
            "changed build relaunch honors argv/cwd/env and loader settings; executable deletion preserves live ownership"
        );
    }
}
