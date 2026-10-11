//! Opt-in main-thread fixture-recognition checks for the frozen AppKit build.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("skipped (not macOS): compiled fixture recognition");
}

#[cfg(target_os = "macos")]
fn main() {
    if !std::env::args().any(|arg| arg == "--ignored" || arg == "--include-ignored") {
        println!("skipped: frozen fixture recognition requires --ignored");
        return;
    }
    native::run();
}

#[cfg(target_os = "macos")]
mod native {
    use glass_core::{AppSpec, Deadline, GlassError, SandboxLevel};
    use glass_macos::window_directed::{OwnedTarget, TargetObservation};
    use std::path::Path;
    use std::time::Duration;

    fn spec(path: &Path) -> AppSpec {
        AppSpec {
            build: None,
            run: vec![path.to_str().unwrap().into(), "stable".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 3_000,
            sandbox: SandboxLevel::Default,
            a11y: true,
        }
    }

    fn first(target: &mut OwnedTarget) -> TargetObservation {
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
                result => return result.expect("observe owned fixture"),
            }
        }
    }

    fn check(launch: &AppSpec, expected: Option<&str>) {
        let mut target = OwnedTarget::launch(launch, &[]).expect("launch owned fixture");
        let observed = first(&mut target);
        println!("{observed:#?}");
        assert_eq!(observed.fixture_profile, expected);
        assert_eq!(observed.startup_context_revalidated, expected.is_some());
        assert!(!observed.configuration_verified);
        assert!(!observed.pixel_geometry_verified);
        assert!(!observed.input_admitted);
        let refreshed = target.observe(Deadline::from_millis(1_000)).unwrap();
        assert_eq!(refreshed.fixture_profile, expected);
        assert_eq!(refreshed.startup_context_revalidated, expected.is_some());
        assert_eq!(refreshed.executable, observed.executable);
        target.stop().expect("confirm owned child cleanup");
        target.stop().unwrap();
    }

    pub fn run() {
        let fixture = std::env::var("GLASS_WINDOW_TARGET_FIXTURE_BIN").unwrap();
        let expected = Some("appkit-ownership-v1");
        check(&spec(Path::new(&fixture)), expected);
        let directory = tempfile::tempdir().unwrap();
        let renamed = directory.path().join("arbitrary-name");
        std::fs::copy(&fixture, &renamed).unwrap();
        check(&spec(&renamed), expected);
        println!("frozen artifact and renamed copy matched; refresh and cleanup passed");

        let mut changed_args = spec(Path::new(&fixture));
        changed_args.run[1] = "unrecognized".into();
        check(&changed_args, None);
        let mut changed_environment = spec(Path::new(&fixture));
        changed_environment.env = vec![("UNRECOGNIZED_FIXTURE_SETTING".into(), "1".into())];
        check(&changed_environment, None);
        println!("changed declared arguments and environment remained unmatched");

        let unknown = std::env::args()
            .nth(2)
            .expect("supply an independently rebuilt unrecognized fixture path after --ignored");
        check(&spec(Path::new(&unknown)), None);
        println!("unrecognized AppKit artifact remained unmatched");
        context_lifecycle(Path::new(&fixture));
        println!("compiled fixture recognition passed; configuration and input remain unqualified");
    }

    fn context_lifecycle(fixture: &Path) {
        let original = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("cwd");
        let old_cwd = directory.path().join("old-cwd");
        std::fs::create_dir(&cwd).unwrap();
        std::env::set_current_dir(&cwd).unwrap();
        let mut target = OwnedTarget::launch(&spec(fixture), &[]).unwrap();
        let observed = first(&mut target);
        assert!(observed.startup_context_revalidated);
        std::env::set_current_dir(&original).unwrap();
        assert!(
            target
                .observe(Deadline::from_millis(1_000))
                .unwrap()
                .startup_context_revalidated
        );
        println!("parent cwd change preserves the bound child startup context");

        std::fs::rename(&cwd, &old_cwd).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        let error = target.observe(Deadline::from_millis(1_000)).unwrap_err();
        assert!(error.to_string().contains("startup"), "{error}");
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
        std::fs::remove_dir(&cwd).unwrap();
        std::fs::rename(&old_cwd, &cwd).unwrap();
        assert!(
            target
                .observe(Deadline::from_millis(1_000))
                .unwrap_err()
                .to_string()
                .contains("invalidated")
        );
        target
            .stop()
            .expect("confirm original context child cleanup");
        target.stop().unwrap();
        println!("cwd replacement refuses before AX and restoration never rebinds; cleanup passed");
    }
}
