use std::process::Command;

use glass_core::{AppSpec, GlassError, Result, Stream};

use crate::logs::LogSink;

/// The host shell program + args for a build command, or `None` if blank.
/// Host-agnostic: `cmd /C` on Windows hosts, `sh -c` elsewhere.
pub fn shell_command(build: &str) -> Option<(String, Vec<String>)> {
    let build = build.trim();
    if build.is_empty() {
        return None;
    }
    #[cfg(windows)]
    let pair = ("cmd".to_string(), vec!["/C".to_string(), build.to_string()]);
    #[cfg(not(windows))]
    let pair = ("sh".to_string(), vec!["-c".to_string(), build.to_string()]);
    Some(pair)
}

/// Run the optional build step on the host (unsandboxed, like glass's desktop build),
/// folding stdout/stderr into the log sink. Errors as `AppNotStarted` on non-zero exit.
pub fn run_build(spec: &AppSpec, sink: &LogSink) -> Result<()> {
    let Some(build) = spec.build.as_deref() else {
        return Ok(());
    };
    let Some((prog, args)) = shell_command(build) else {
        return Ok(());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .map_err(|e| GlassError::AppNotStarted(format!("build `{build}` failed to start: {e}")))?;
    push_lines(sink, Stream::Stdout, &out.stdout);
    push_lines(sink, Stream::Stderr, &out.stderr);
    if out.status.success() {
        Ok(())
    } else {
        Err(GlassError::AppNotStarted(format!(
            "build `{build}` exited with {}",
            out.status
        )))
    }
}

fn push_lines(sink: &LogSink, stream: Stream, bytes: &[u8]) {
    if let Ok(mut g) = sink.lock() {
        for line in String::from_utf8_lossy(bytes).lines() {
            g.push((stream, line.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A spec whose only interesting field is the build command — the rest is what the launch
    /// would need, and `run_build` never reaches it.
    #[cfg(unix)]
    fn spec_that_builds_with(build: &str) -> AppSpec {
        AppSpec {
            build: Some(build.to_string()),
            run: vec!["com.example.app/.MainActivity".to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 15_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        }
    }

    /// Both streams of a build, as `drain_logs` would hand them to the caller.
    #[cfg(unix)]
    fn run_and_collect(build: &str) -> (Result<()>, Vec<(Stream, String)>) {
        let sink: LogSink = Arc::new(Mutex::new(Vec::new()));
        let outcome = run_build(&spec_that_builds_with(build), &sink);
        let lines = sink.lock().expect("sink lock").clone();
        (outcome, lines)
    }

    #[test]
    #[cfg(unix)]
    fn a_build_that_fails_says_so_and_keeps_what_it_printed() {
        // A build that reports success is an app launched against stale artifacts, and the
        // compiler error explaining why is in the output this folds into the log.
        let (outcome, lines) =
            run_and_collect("printf 'made it\\n'; printf 'boom\\n' 1>&2; exit 3");
        let err = outcome.expect_err("a non-zero build must not read as a launch that can go on");
        assert!(matches!(err, GlassError::AppNotStarted(_)), "{err}");
        assert!(
            lines.contains(&(Stream::Stdout, "made it".to_string())),
            "{lines:?}"
        );
        assert!(
            lines.contains(&(Stream::Stderr, "boom".to_string())),
            "{lines:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_build_that_succeeds_is_ok_and_still_keeps_its_output() {
        let (outcome, lines) = run_and_collect("printf 'built\\n'");
        outcome.expect("a zero exit is a build that worked");
        assert!(
            lines.contains(&(Stream::Stdout, "built".to_string())),
            "{lines:?}"
        );
    }

    #[test]
    fn empty_build_is_a_noop_command() {
        assert!(shell_command("").is_none());
        assert!(shell_command("   ").is_none());
    }

    #[test]
    fn non_empty_build_yields_program_and_args() {
        let (prog, args) = shell_command("./gradlew assembleDebug").unwrap();
        #[cfg(windows)]
        {
            assert_eq!(prog, "cmd");
            assert_eq!(
                args,
                vec!["/C".to_string(), "./gradlew assembleDebug".to_string()]
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(prog, "sh");
            assert_eq!(
                args,
                vec!["-c".to_string(), "./gradlew assembleDebug".to_string()]
            );
        }
    }
}
