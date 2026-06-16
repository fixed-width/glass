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
        Err(GlassError::AppNotStarted(format!("build `{build}` exited with {}", out.status)))
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
            assert_eq!(args, vec!["/C".to_string(), "./gradlew assembleDebug".to_string()]);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(prog, "sh");
            assert_eq!(args, vec!["-c".to_string(), "./gradlew assembleDebug".to_string()]);
        }
    }
}
