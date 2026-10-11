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
        assert!(!observed.configuration_verified);
        assert!(!observed.pixel_geometry_verified);
        assert!(!observed.input_admitted);
        let refreshed = target.observe(Deadline::from_millis(1_000)).unwrap();
        assert_eq!(refreshed.fixture_profile, expected);
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
        println!("compiled fixture recognition passed; configuration and input remain unqualified");
    }
}
