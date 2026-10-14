//! Run the requested build with its working directory and environment.

use glass_core::{AppSpec, GlassError, Result};

pub(crate) fn run(spec: &AppSpec) -> Result<()> {
    let Some(build) = &spec.build else {
        return Ok(());
    };
    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(build)
        .envs(spec.env.iter().map(|(key, value)| (key, value)));
    if let Some(directory) = &spec.cwd {
        command.current_dir(directory);
    }
    let status = command
        .status()
        .map_err(|error| GlassError::AppNotStarted(format!("build command: {error}")))?;
    if !status.success() {
        return Err(GlassError::AppNotStarted(format!(
            "build command failed with status {status}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn spec(directory: &std::path::Path) -> AppSpec {
        AppSpec {
            build: None,
            run: Vec::new(),
            cwd: Some(directory.to_path_buf()),
            env: Vec::new(),
            window_hint: None,
            timeout_ms: 1000,
            sandbox: glass_core::SandboxLevel::Default,
            a11y: false,
        }
    }

    #[test]
    fn requested_environment_reaches_build_in_requested_directory() {
        let directory = tempfile::tempdir().unwrap();
        let mut launch = spec(directory.path());
        launch.build = Some("printf '%s' \"$GLASS_BUILD_TEST_VALUE\" > build-result".into());
        launch.env = vec![(
            "GLASS_BUILD_TEST_VALUE".into(),
            "dependency Ω\nsecond line".into(),
        )];
        run(&launch).unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.path().join("build-result")).unwrap(),
            "dependency Ω\nsecond line"
        );
    }

    #[test]
    fn requested_path_selects_build_tool() {
        let directory = tempfile::tempdir().unwrap();
        let tools = directory.path().join("toolchain");
        std::fs::create_dir(&tools).unwrap();
        let tool = tools.join("glass-build-test-tool");
        std::fs::write(&tool, "#!/bin/sh\nprintf '%s' toolchain > tool-result\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut launch = spec(directory.path());
        launch.build = Some("glass-build-test-tool".into());
        launch.env = vec![("PATH".into(), tools.to_string_lossy().into_owned())];
        run(&launch).unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.path().join("tool-result")).unwrap(),
            "toolchain"
        );
    }

    #[test]
    fn failed_build_retains_exit_status() {
        let directory = tempfile::tempdir().unwrap();
        let mut launch = spec(directory.path());
        launch.build = Some("exit 23".into());
        assert!(
            matches!(run(&launch), Err(GlassError::AppNotStarted(message)) if message.contains("23"))
        );
    }

    #[test]
    fn missing_build_does_not_use_working_directory() {
        assert!(run(&spec(std::path::Path::new("/nonexistent-glass-build-test"))).is_ok());
    }
}
