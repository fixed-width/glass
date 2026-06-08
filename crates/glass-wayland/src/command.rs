use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use glass_core::{AppSpec, SandboxLevel, Stream};
use glass_sandbox_linux::{ephemeral_home, wrap_argv, WrapOpts};

pub type LogSink = Arc<Mutex<Vec<(Stream, String)>>>;

/// Headless output size for the spawned sway compositor (matches the prior cage
/// output size, which existing tests assert).
pub const OUTPUT_WIDTH: u32 = 1280;
pub const OUTPUT_HEIGHT: u32 = 720;

/// Render a minimal per-session sway config: one headless output at a fixed size,
/// no window borders, every window floating (so toplevels keep their natural size
/// for true per-window capture/geometry), and an `exec` that launches the target
/// app. `spec.run` args are shell-quoted because sway runs `exec` through
/// `/bin/sh -c`.
///
/// When `spec.sandbox` is not `Off`, the `exec` argv is wrapped in a `bwrap`
/// invocation so the launched process runs in a sandboxed user namespace. The
/// Wayland socket dir (`runtime_dir`) is re-exposed read-write inside the
/// namespace so the app can still connect to sway.
pub fn sway_config(spec: &AppSpec, runtime_dir: &Path) -> String {
    let argv: Vec<String> = match spec.sandbox {
        SandboxLevel::Off => spec.run.to_vec(),
        level => {
            let prog = OsString::from(&spec.run[0]);
            let args: Vec<OsString> = spec.run[1..].iter().map(OsString::from).collect();
            // Re-expose the program binary when it is absolute (it may live under
            // $HOME, which the ephemeral tmpfs shadows). PATH-resolved bare names
            // are covered by `--ro-bind / /` and need no extra bind.
            let opts = WrapOpts {
                level,
                home: ephemeral_home(),
                cwd: spec.cwd.clone(),
                ro_binds: glass_sandbox_linux::program_ro_binds(&prog),
                rw_binds: vec![runtime_dir.to_path_buf()],
            };
            let wrapped = wrap_argv(&prog, &args, &opts);
            // sway's config is a text file, so argv elements must be Strings.
            // Every element here is an ASCII bwrap flag, a glass-owned path
            // (runtime_dir / ephemeral HOME / the program), or spec.run (already
            // String) — all valid UTF-8 in practice; to_string_lossy is the
            // pragmatic conversion (a non-UTF-8 path would make bwrap fail
            // loudly, not escape silently).
            wrapped.into_iter().map(|s| s.to_string_lossy().into_owned()).collect()
        }
    };
    let exec = argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    format!(
        "output HEADLESS-1 resolution {OUTPUT_WIDTH}x{OUTPUT_HEIGHT}\n\
         default_border none\n\
         for_window [title=\".*\"] floating enable\n\
         exec {exec}\n"
    )
}

/// Single-quote a string for a `/bin/sh` command line (escape embedded quotes).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build `sway --unsupported-gpu -c <config>` headless, with a private
/// `XDG_RUNTIME_DIR`. `--unsupported-gpu` is required because sway refuses to
/// start on proprietary-Nvidia hosts; it is harmless under the headless backend.
/// `spec.env` is applied last so a caller can still override anything.
pub fn build_sway_command(sway: &Path, config: &Path, spec: &AppSpec, runtime_dir: &Path) -> Command {
    let mut cmd = Command::new(sway);
    // Run sway as its own process-group leader so the whole compositor subtree
    // it spawns (Xwayland + the exec'd app) can be torn down as a group on stop;
    // a bare SIGKILL of just the sway pid would orphan those children.
    cmd.process_group(0);
    cmd.arg("--unsupported-gpu");
    cmd.arg("-c").arg(config);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("WLR_BACKENDS", "headless");
    cmd.env("WLR_LIBINPUT_NO_DEVICES", "1");
    // Software-GL fallback so the headless compositor renders with no GPU.
    cmd.env("WLR_RENDERER_ALLOW_SOFTWARE", "1");
    // Isolate from any host Wayland/X11 display the glass process inherited.
    cmd.env_remove("WAYLAND_DISPLAY");
    cmd.env_remove("DISPLAY");
    cmd.env_remove("WAYLAND_SOCKET");
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd
}

/// Pipe a child stream's lines into the shared log sink (background thread).
pub fn spawn_reader<R: std::io::Read + Send + 'static>(reader: R, stream: Stream, sink: LogSink) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(text) => sink.lock().unwrap().push((stream, text)),
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn spec(run: &[&str]) -> AppSpec {
        AppSpec {
            build: None,
            run: run.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1000,
            sandbox: glass_core::SandboxLevel::Off,
        }
    }

    #[test]
    fn sway_config_has_output_border_and_quoted_exec() {
        // sandbox: Off — exec must be the bare app argv, not wrapped in bwrap.
        let cfg = sway_config(&spec(&["glass-testapp", "--windows", "2"]), std::path::Path::new("/run/glass-rt"));
        assert!(cfg.contains("output HEADLESS-1 resolution 1280x720"), "{cfg}");
        assert!(cfg.contains("default_border none"), "{cfg}");
        assert!(cfg.contains("floating enable"), "{cfg}");
        assert!(cfg.contains("exec 'glass-testapp' '--windows' '2'"), "{cfg}");
    }

    #[test]
    fn sway_config_exec_is_bwrap_wrapped_when_sandboxed() {
        use glass_core::SandboxLevel;
        let mut s = spec(&["glass-testapp", "--windows", "2"]);
        s.sandbox = SandboxLevel::Default;
        let cfg = sway_config(&s, std::path::Path::new("/run/glass-rt"));
        assert!(cfg.contains("exec 'bwrap'"), "{cfg}");
        assert!(cfg.contains("'--bind-try' '/run/glass-rt' '/run/glass-rt'"), "{cfg}");
        assert!(cfg.contains("'--' 'glass-testapp' '--windows' '2'"), "{cfg}");
    }

    #[test]
    fn sway_config_exec_unwrapped_when_off() {
        let cfg = sway_config(&spec(&["app"]), std::path::Path::new("/run/glass-rt"));
        assert!(cfg.contains("exec 'app'"), "{cfg}");
        assert!(!cfg.contains("bwrap"), "{cfg}");
    }

    #[test]
    fn build_sway_command_args_and_headless_env() {
        let cmd = build_sway_command(
            Path::new("/opt/glass/sway/bin/sway"),
            Path::new("/run/x/sway.cfg"),
            &spec(&["app"]),
            Path::new("/run/x"),
        );
        assert_eq!(cmd.get_program(), OsStr::new("/opt/glass/sway/bin/sway"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![OsStr::new("--unsupported-gpu"), OsStr::new("-c"), OsStr::new("/run/x/sway.cfg")]
        );
        // Collect envs PRESERVING removals: get_envs yields (key, None) for env_remove.
        let envs: std::collections::HashMap<std::ffi::OsString, Option<std::ffi::OsString>> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();
        assert_eq!(envs.get(OsStr::new("WLR_BACKENDS")), Some(&Some(OsStr::new("headless").to_owned())));
        assert_eq!(
            envs.get(OsStr::new("WLR_RENDERER_ALLOW_SOFTWARE")),
            Some(&Some(OsStr::new("1").to_owned()))
        );
        for removed in ["WAYLAND_DISPLAY", "DISPLAY", "WAYLAND_SOCKET"] {
            assert_eq!(envs.get(OsStr::new(removed)), Some(&None), "{removed} must be removed");
        }
    }
}
