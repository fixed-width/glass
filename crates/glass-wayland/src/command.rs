use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use glass_core::{AppSpec, SandboxLevel, Stream};
use glass_sandbox_linux::{ephemeral_home, wrap_argv, WrapOpts};

pub type LogSink = Arc<Mutex<Vec<(Stream, String)>>>;

/// Default headless output size for the spawned sway compositor. Matches the
/// X11 backend's `GLASS_XVFB_SCREEN` default (1280x800) so both backends present
/// the same screen unless overridden. Override with `GLASS_WAYLAND_SCREEN` (see
/// [`output_resolution`]).
pub const OUTPUT_WIDTH: u32 = 1280;
pub const OUTPUT_HEIGHT: u32 = 800;

/// Parse a `WxH` screen spec (e.g. `"1920x1080"`) into `(width, height)`.
/// Returns `None` for anything malformed — missing/extra `x`, non-numeric, or a
/// zero dimension — so the caller falls back to the default rather than emitting
/// a broken `output` line. Note the contract differs from X11's
/// `GLASS_XVFB_SCREEN` (`WxHxDepth`): a headless wlroots output has no
/// caller-chosen color depth, so the depth field is intentionally rejected.
fn parse_screen(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// The headless output resolution: `GLASS_WAYLAND_SCREEN` (`WxH`) when set and
/// well-formed, otherwise the [`OUTPUT_WIDTH`]×[`OUTPUT_HEIGHT`] default.
fn output_resolution() -> (u32, u32) {
    std::env::var("GLASS_WAYLAND_SCREEN")
        .ok()
        .and_then(|s| parse_screen(&s))
        .unwrap_or((OUTPUT_WIDTH, OUTPUT_HEIGHT))
}

/// Render a minimal per-session sway config: one headless output sized by
/// [`output_resolution`],
/// no window borders, every window floating (so toplevels keep their natural size
/// for true per-window capture/geometry), and an `exec` that launches the target
/// app. `spec.run` args are shell-quoted because sway runs `exec` through
/// `/bin/sh -c`.
///
/// When `spec.sandbox` is not `Off`, the `exec` argv is wrapped in a `bwrap`
/// invocation so the launched process runs in a sandboxed user namespace. The
/// Wayland socket dir (`runtime_dir`) is re-exposed read-write inside the
/// namespace so the app can still connect to sway.
pub fn sway_config(spec: &AppSpec, runtime_dir: &Path, a11y_bind_dir: Option<&Path>) -> String {
    let argv: Vec<String> = match spec.sandbox {
        SandboxLevel::Off => spec.run.to_vec(),
        level => {
            let prog = OsString::from(&spec.run[0]);
            let args: Vec<OsString> = spec.run[1..].iter().map(OsString::from).collect();
            // Re-expose the program binary when it is absolute (it may live under
            // $HOME, which the ephemeral tmpfs shadows). PATH-resolved bare names
            // are covered by `--ro-bind / /` and need no extra bind.
            let mut ro_binds = glass_sandbox_linux::program_ro_binds(&prog);
            if let Some(dir) = a11y_bind_dir {
                ro_binds.push(dir.to_path_buf());
            }
            let opts = WrapOpts {
                level,
                home: ephemeral_home(),
                cwd: spec.cwd.clone(),
                ro_binds,
                rw_binds: vec![runtime_dir.to_path_buf()],
            };
            let wrapped = wrap_argv(&prog, &args, &opts);
            // sway's config is a text file, so argv elements must be Strings.
            // Every element here is an ASCII bwrap flag, a glass-owned path
            // (runtime_dir / ephemeral HOME / the program), or spec.run (already
            // String) — all valid UTF-8 in practice; to_string_lossy is the
            // pragmatic conversion (a non-UTF-8 path would make bwrap fail
            // loudly, not escape silently).
            wrapped
                .into_iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect()
        }
    };
    let exec = argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let (out_w, out_h) = output_resolution();
    format!(
        "output HEADLESS-1 resolution {out_w}x{out_h}\n\
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
pub fn build_sway_command(
    sway: &Path,
    config: &Path,
    spec: &AppSpec,
    runtime_dir: &Path,
    dbus_addr: Option<&str>,
) -> Command {
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
    if let Some(addr) = dbus_addr {
        // sway passes its env to the exec'd app (like XDG_RUNTIME_DIR); under a sandbox the
        // exec's bwrap inherits it too (no --clearenv). spec.env below still overrides.
        cmd.env("DBUS_SESSION_BUS_ADDRESS", addr);
    }
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
                Ok(text) => sink.lock().expect("log sink mutex").push((stream, text)),
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
            a11y: false,
        }
    }

    #[test]
    fn parse_screen_accepts_wxh() {
        assert_eq!(parse_screen("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_screen("1280x720"), Some((1280, 720)));
    }

    #[test]
    fn parse_screen_rejects_malformed_and_falls_back() {
        // Missing 'x', non-numeric, and zero dimensions are malformed -> None,
        // so the caller keeps the default rather than emitting a broken output line.
        for bad in ["1920", "axb", "0x600", "800x0", "", "x", "1280x"] {
            assert_eq!(parse_screen(bad), None, "{bad:?} should be rejected");
        }
    }

    #[test]
    fn parse_screen_rejects_xvfb_style_depth() {
        // Unlike X11's GLASS_XVFB_SCREEN (WxHxDepth), GLASS_WAYLAND_SCREEN is WxH:
        // a headless wlroots output has no caller-chosen depth. Reject the triple
        // form loudly instead of silently ignoring the depth field.
        assert_eq!(parse_screen("1280x800x24"), None);
    }

    #[test]
    fn sway_config_has_output_border_and_quoted_exec() {
        // sandbox: Off — exec must be the bare app argv, not wrapped in bwrap.
        let cfg = sway_config(
            &spec(&["glass-testapp", "--windows", "2"]),
            std::path::Path::new("/run/glass-rt"),
            None,
        );
        assert!(
            cfg.contains("output HEADLESS-1 resolution 1280x800"),
            "{cfg}"
        );
        assert!(cfg.contains("default_border none"), "{cfg}");
        assert!(cfg.contains("floating enable"), "{cfg}");
        assert!(
            cfg.contains("exec 'glass-testapp' '--windows' '2'"),
            "{cfg}"
        );
    }

    #[test]
    fn sway_config_exec_is_bwrap_wrapped_when_sandboxed() {
        use glass_core::SandboxLevel;
        let mut s = spec(&["glass-testapp", "--windows", "2"]);
        s.sandbox = SandboxLevel::Default;
        let cfg = sway_config(&s, std::path::Path::new("/run/glass-rt"), None);
        assert!(cfg.contains("exec 'bwrap'"), "{cfg}");
        assert!(
            cfg.contains("'--bind-try' '/run/glass-rt' '/run/glass-rt'"),
            "{cfg}"
        );
        assert!(
            cfg.contains("'--' 'glass-testapp' '--windows' '2'"),
            "{cfg}"
        );
    }

    #[test]
    fn sway_config_exec_unwrapped_when_off() {
        let cfg = sway_config(&spec(&["app"]), std::path::Path::new("/run/glass-rt"), None);
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
            None,
        );
        assert_eq!(cmd.get_program(), OsStr::new("/opt/glass/sway/bin/sway"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                OsStr::new("--unsupported-gpu"),
                OsStr::new("-c"),
                OsStr::new("/run/x/sway.cfg")
            ]
        );
        // Collect envs PRESERVING removals: get_envs yields (key, None) for env_remove.
        let envs: std::collections::HashMap<std::ffi::OsString, Option<std::ffi::OsString>> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();
        assert_eq!(
            envs.get(OsStr::new("WLR_BACKENDS")),
            Some(&Some(OsStr::new("headless").to_owned()))
        );
        assert_eq!(
            envs.get(OsStr::new("WLR_RENDERER_ALLOW_SOFTWARE")),
            Some(&Some(OsStr::new("1").to_owned()))
        );
        for removed in ["WAYLAND_DISPLAY", "DISPLAY", "WAYLAND_SOCKET"] {
            assert_eq!(
                envs.get(OsStr::new(removed)),
                Some(&None),
                "{removed} must be removed"
            );
        }
    }

    #[test]
    fn build_sway_command_injects_dbus_addr() {
        let s = spec(&["app"]);
        let cmd = build_sway_command(
            std::path::Path::new("/usr/bin/sway"),
            std::path::Path::new("/tmp/cfg"),
            &s,
            std::path::Path::new("/run/glass-rt"),
            Some("unix:path=/tmp/glass-a11y/session-bus"),
        );
        let dbus = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("DBUS_SESSION_BUS_ADDRESS"))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned());
        assert_eq!(
            dbus.as_deref(),
            Some("unix:path=/tmp/glass-a11y/session-bus")
        );
    }

    #[test]
    fn sway_config_binds_a11y_dir_when_sandboxed() {
        let mut s = spec(&["app"]);
        s.sandbox = glass_core::SandboxLevel::Default;
        let cfg = sway_config(
            &s,
            std::path::Path::new("/run/glass-rt"),
            Some(std::path::Path::new("/tmp/glass-a11y-xyz")),
        );
        assert!(
            cfg.contains("/tmp/glass-a11y-xyz"),
            "a11y dir not bound into the exec bwrap:\n{cfg}"
        );
    }
}
