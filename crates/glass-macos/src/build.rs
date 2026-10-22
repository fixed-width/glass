//! Run the requested build with its working directory and environment.

use std::collections::VecDeque;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

use glass_core::{AppSpec, GlassError, Result, Stream};
use glass_pipe_unix::{ChunkSink, PipeTap};

const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Default)]
struct Tail {
    bytes: VecDeque<u8>,
    truncated: bool,
}

struct Capture(Arc<Mutex<Tail>>);
impl ChunkSink for Capture {
    fn chunk(&mut self, bytes: &[u8]) {
        let mut tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let overflow = tail
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(OUTPUT_LIMIT);
        tail.truncated |= overflow > 0;
        let discard = overflow.min(tail.bytes.len());
        tail.bytes.drain(..discard);
        tail.bytes
            .extend(bytes.iter().skip(overflow - discard).copied());
    }
}

fn output(tail: &Mutex<Tail>) -> String {
    let tail = tail
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bytes: Vec<_> = tail.bytes.iter().copied().collect();
    let prefix = if tail.truncated {
        "[earlier build output omitted]\n"
    } else {
        ""
    };
    format!("{prefix}{}", String::from_utf8_lossy(&bytes))
}

fn tap<R: std::io::Read + std::os::fd::AsFd + Send + 'static>(
    pipe: R,
    name: &str,
    capture: &Arc<Mutex<Tail>>,
    child: &mut Child,
) -> Result<PipeTap> {
    PipeTap::start(pipe, name, Capture(Arc::clone(capture))).map_err(|error| {
        let stopped = child.kill().and_then(|()| child.wait().map(|_| ()));
        let primary = GlassError::AppNotStarted(format!("reading build output: {error}"));
        match stopped {
            Ok(()) => primary,
            Err(cleanup) => GlassError::cleanup_failed(
                "stopping build after output reader failure",
                primary,
                cleanup.into(),
            ),
        }
    })
}

pub(crate) fn run(spec: &AppSpec) -> Result<Vec<(Stream, String)>> {
    let Some(build) = &spec.build else {
        return Ok(Vec::new());
    };
    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(build)
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(directory) = &spec.cwd {
        command.current_dir(directory);
    }
    let mut child = command
        .spawn()
        .map_err(|error| GlassError::AppNotStarted(format!("build command: {error}")))?;
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::private_session::work::record(child.id()) {
        let stopped = child.kill().and_then(|()| child.wait().map(|_| ()));
        let primary = GlassError::AppNotStarted(format!("recording build ownership: {error}"));
        return Err(match stopped {
            Ok(()) => primary,
            Err(cleanup) => {
                GlassError::cleanup_failed("stopping unrecorded build", primary, cleanup.into())
            }
        });
    }
    let stdout = Arc::new(Mutex::new(Tail::default()));
    let stderr = Arc::new(Mutex::new(Tail::default()));
    let out = child
        .stdout
        .take()
        .ok_or_else(|| GlassError::Backend("build stdout pipe missing".into()))?;
    let err = child
        .stderr
        .take()
        .ok_or_else(|| GlassError::Backend("build stderr pipe missing".into()))?;
    let taps = [
        tap(out, "glass-build-out", &stdout, &mut child)?,
        tap(err, "glass-build-err", &stderr, &mut child)?,
    ];
    let status = child.wait();
    drop(taps);
    let status =
        status.map_err(|error| GlassError::AppNotStarted(format!("waiting for build: {error}")))?;
    let output = [
        (Stream::Stdout, output(&stdout)),
        (Stream::Stderr, output(&stderr)),
    ];
    if !status.success() {
        let [(_, stdout), (_, stderr)] = output;
        return Err(GlassError::BuildFailed {
            status: status.to_string(),
            stdout,
            stderr,
        });
    }
    Ok(output
        .into_iter()
        .flat_map(|(stream, text)| {
            text.lines()
                .map(|line| (stream, line.to_owned()))
                .collect::<Vec<_>>()
        })
        .collect())
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
        launch.build = Some("printf '%s' \"$BUILD_TEST_VALUE\" > build-result".into());
        launch.env = vec![(
            "BUILD_TEST_VALUE".into(),
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
            matches!(run(&launch), Err(GlassError::BuildFailed { status, .. }) if status.contains("23"))
        );
    }

    #[test]
    fn missing_build_does_not_use_working_directory() {
        assert!(run(&spec(std::path::Path::new("/nonexistent-glass-build-test"))).is_ok());
    }
    #[test]
    fn build_output_is_captured_outside_the_backend_protocol() {
        let directory = tempfile::tempdir().unwrap();
        let mut launch = spec(directory.path());
        launch.build = Some("printf 'compiler output\\n'; printf 'warning\\n' >&2".into());
        let logs = run(&launch).unwrap();
        assert!(logs.contains(&(Stream::Stdout, "compiler output".into())));
        assert!(logs.contains(&(Stream::Stderr, "warning".into())));
    }

    #[test]
    fn failed_build_exposes_its_diagnostic_tail() {
        let directory = tempfile::tempdir().unwrap();
        let mut launch = spec(directory.path());
        launch.build = Some("printf 'compiler failure' >&2; exit 23".into());
        let error = run(&launch).unwrap_err();
        assert!(!error.to_string().contains("compiler failure"));
        assert!(error.to_string().contains("23"));
        let GlassError::BuildFailed { stderr, .. } = error else {
            panic!("build failure lost its diagnostic data");
        };
        assert_eq!(stderr, "compiler failure");
    }

    #[test]
    fn build_capture_bounds_a_long_unterminated_stream() {
        let capture = Arc::new(Mutex::new(Tail::default()));
        let mut sink = Capture(Arc::clone(&capture));
        sink.chunk(&vec![b'x'; OUTPUT_LIMIT * 2]);
        sink.chunk(b"tail");
        assert_eq!(capture.lock().unwrap().bytes.len(), OUTPUT_LIMIT);
        assert!(output(&capture).starts_with("[earlier build output omitted]"));
        assert!(output(&capture).ends_with("tail"));
    }
}
