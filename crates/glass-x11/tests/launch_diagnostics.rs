#![cfg(target_os = "linux")]

use glass_core::{AppSpec, Backend, BaselineStore, Glass, GlassError, SandboxLevel, WindowHint};

#[test]
#[ignore = "starts private Xvfb displays and shell processes"]
fn failed_starts_distinguish_spawn_exit_and_window_timeout_and_retain_output() {
    let directory = tempfile::tempdir().unwrap();
    let xvfb = glass_x11::Xvfb::start("1280x800x24").expect("private test display");
    let display = xvfb.display.clone();
    // Keep Xvfb from resetting between failed launches when the last client disconnects.
    let _display_connection = glass_x11::X11Platform::connect(Some(&display)).unwrap();
    let mut glass = Glass::new(
        Box::new(move |_| {
            Ok(Backend::display_only(Box::new(
                glass_x11::X11Platform::connect(Some(&display))?,
            )))
        }),
        "x11".into(),
        BaselineStore::new(directory.path()),
        100,
    );
    let spec = AppSpec {
        build: None,
        run: vec!["/nonexistent/glass-helper".into()],
        cwd: None,
        env: vec![],
        window_hint: Some(WindowHint {
            title: Some("Never published".into()),
            class: None,
        }),
        timeout_ms: 200,
        sandbox: SandboxLevel::Off,
        a11y: false,
    };
    let error = glass
        .start(&spec)
        .expect_err("missing executable must fail");
    assert!(matches!(error, GlassError::AppNotStarted(_)), "{error}");
    assert_eq!(glass.failed_launch().unwrap().phase, "spawn");

    let mut missing_script = spec.clone();
    missing_script.run = vec![
        "/bin/sh".into(),
        directory
            .path()
            .join("missing.sh")
            .to_string_lossy()
            .into_owned(),
    ];
    let error = glass
        .start(&missing_script)
        .expect_err("missing helper must fail");
    assert!(matches!(error, GlassError::AppExited(Some(2))), "{error}");
    assert!(
        glass
            .logs(0, 100, None, None)
            .unwrap()
            .0
            .iter()
            .any(|line| line.text.contains("missing.sh"))
    );

    let mut exiting = spec.clone();
    exiting.run = vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf 'stdout evidence\\n'; printf 'stderr evidence\\n' >&2; exit 23".into(),
    ];
    let error = glass.start(&exiting).expect_err("exited process must fail");
    assert!(matches!(error, GlassError::AppExited(Some(23))), "{error}");
    let (logs, _) = glass.logs(0, 100, None, None).unwrap();
    assert!(logs.iter().any(|line| line.text == "stdout evidence"));
    assert!(logs.iter().any(|line| line.text == "stderr evidence"));
    assert_eq!(glass.failed_launch().unwrap().exit_code, Some(23));

    let mut waiting = spec;
    waiting.run = vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf 'waiting for window\\n'; exec sleep 60".into(),
    ];
    let error = glass
        .start(&waiting)
        .expect_err("unpublished window must time out");
    assert!(matches!(error, GlassError::Timeout(200)), "{error}");
    let (logs, _) = glass.logs(0, 100, None, None).unwrap();
    assert!(logs.iter().any(|line| line.text == "waiting for window"));
    assert!(!logs.iter().any(|line| line.text == "stdout evidence"));
    let diagnostics = glass.failed_launch().unwrap();
    assert_eq!(diagnostics.phase, "window_discovery");
    assert_eq!(diagnostics.window_hint, waiting.window_hint);
}
