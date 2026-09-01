use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use glass_core::{AppSpec, ProtectedHostPath, Result, SandboxLevel, Stream};
use glass_sandbox_linux::{WrapOpts, ephemeral_home, wrap_argv};

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

/// Render a minimal per-session sway config as exact Unix bytes: one headless output sized by
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
pub fn sway_config(
    spec: &AppSpec,
    runtime_dir: &Path,
    a11y_bind_dir: Option<&Path>,
    status_fd: Option<i32>,
    protected_paths: &[ProtectedHostPath],
) -> Result<Vec<u8>> {
    let argv: Vec<OsString> = match spec.sandbox {
        SandboxLevel::Off => spec.run.iter().map(OsString::from).collect(),
        level => {
            let prog = OsString::from(&spec.run[0]);
            let args: Vec<OsString> = spec.run[1..].iter().map(OsString::from).collect();
            // Default the working directory to glass's own cwd when the spec sets none, so a
            // contained launch with no `cwd` still gets `--chdir` + a guarded rw bind of that
            // directory and any relative launch token resolves against it. Computed ONCE and
            // shared by both consumers verbatim. If `current_dir()` fails, both see `None` —
            // no `--chdir`/bind, and relative tokens are skipped rather than resolved against
            // a wrong root.
            let effective_cwd = spec.cwd.clone().or_else(|| {
                std::env::current_dir()
                    .inspect_err(|e| {
                        eprintln!(
                            "glass: could not resolve a default cwd for the sandboxed launch: {e}; \
                             relative launch tokens may not resolve"
                        )
                    })
                    .ok()
            });
            let home_os = ephemeral_home();
            // Always re-expose the X11 socket dir: an Xwayland client reaches the display over
            // /tmp/.X11-unix/X<n>, which the ephemeral /tmp tmpfs shadows. Clients that fall back
            // to the abstract socket (`@/tmp/.X11-unix/X<n>`) survive without it, but not every
            // X11 stack still tries abstract sockets, so bind the real one like glass-x11 does.
            // Also re-expose the launch target — the program and any path token under $HOME or
            // /tmp, which the ephemeral tmpfs shadows.
            let mut ro_binds = vec![PathBuf::from("/tmp/.X11-unix")];
            ro_binds.extend(glass_sandbox_linux::launch_ro_binds(
                &prog,
                &args,
                Path::new(&home_os),
                effective_cwd.as_deref(),
            ));
            if let Some(dir) = a11y_bind_dir {
                ro_binds.push(dir.to_path_buf());
            }
            let opts = WrapOpts {
                level,
                home: home_os,
                cwd: effective_cwd,
                ro_binds,
                rw_binds: vec![runtime_dir.to_path_buf()],
                status_fd,
                protected_paths: protected_paths.to_vec(),
            };
            wrap_argv(&prog, &args, &opts)?
        }
    };
    Ok(render_sway_config(&argv))
}

fn render_sway_config(argv: &[OsString]) -> Vec<u8> {
    let (out_w, out_h) = output_resolution();
    let mut config = format!(
        "output HEADLESS-1 resolution {out_w}x{out_h}\n\
         default_border none\n\
         for_window [title=\".*\"] floating enable\n\
         exec "
    )
    .into_bytes();
    for (index, argument) in argv.iter().enumerate() {
        if index > 0 {
            config.push(b' ');
        }
        shell_quote(argument, &mut config);
    }
    config.push(b'\n');
    config
}

/// Ask the kernel to signal the compositor if the *thread* that launched it dies — this crate's
/// own unit tests only.
///
/// `cargo mutants` SIGKILLs the test process when a mutant exceeds its timeout, so a mutation
/// that removes a bound hangs the run holding live sessions and orphans every compositor in
/// flight. Each holds an X display number, and wlroots searches only a bounded range for a free
/// one — leak enough and no session can start Xwayland, after which mutants are graded on that.
///
/// Not production, and not for the tempting reason: `PR_SET_PDEATHSIG` is thread-scoped, but
/// glass-mcp already runs every tool body on one long-lived thread for exactly that
/// (`glass-mcp/src/server.rs`). It is out because this crate is a library and its caller's
/// threading is not its to assume.
///
/// TERM rather than KILL, so sway still removes its sockets and reaps its Xwayland and client.
#[cfg(test)]
fn die_with_launcher(cmd: &mut Command) {
    // SAFETY: the closure runs in the forked child before exec. `prctl` is a bare syscall — it
    // allocates nothing and takes no lock, so it is safe in that window.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))
                .map_err(std::io::Error::from)
        });
    }
}

#[cfg(not(test))]
fn die_with_launcher(_: &mut Command) {}

/// Single-quote an OS-native argument for `/bin/sh`, escaping embedded apostrophes.
fn shell_quote(argument: &std::ffi::OsStr, output: &mut Vec<u8>) {
    output.push(b'\'');
    for byte in argument.as_bytes() {
        if *byte == b'\'' {
            output.extend_from_slice(b"'\\''");
        } else {
            output.push(*byte);
        }
    }
    output.push(b'\'');
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
    build_sway_command_inner(sway, config, spec, runtime_dir, dbus_addr)
}

/// Build sway while declaring the Bubblewrap status descriptor that must remain open across the
/// sway exec. The descriptor is created inheritable by [`glass_sandbox_linux::BwrapStatusPipe`];
/// keeping the pipe alive through `Command::spawn` carries it through sway to the config's exec.
pub fn build_sway_command_with_status(
    sway: &Path,
    config: &Path,
    spec: &AppSpec,
    runtime_dir: &Path,
    dbus_addr: Option<&str>,
    status_fd: Option<i32>,
) -> Result<Command> {
    match (spec.sandbox, status_fd) {
        (SandboxLevel::Off, None) | (SandboxLevel::Default | SandboxLevel::Strict, Some(0..)) => {}
        (SandboxLevel::Off, Some(_)) => {
            return Err(glass_core::GlassError::Backend(
                "sandbox-off sway launch received a Bubblewrap status descriptor".into(),
            ));
        }
        (SandboxLevel::Default | SandboxLevel::Strict, _) => {
            return Err(glass_core::GlassError::SandboxUnavailable(
                "contained sway launch requires an inheritable Bubblewrap status descriptor".into(),
            ));
        }
    }
    Ok(build_sway_command_inner(
        sway,
        config,
        spec,
        runtime_dir,
        dbus_addr,
    ))
}

fn build_sway_command_inner(
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
    die_with_launcher(&mut cmd);
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
    // Apply the same software-render defaults as X11, for consistency and as a safe default under
    // the headless compositor (which already software-renders via WLR_RENDERER_ALLOW_SOFTWARE
    // above). A native-Wayland client presents via wl_shm/memfd rather than X11 MIT-SHM, so the
    // X11 black-frame cause may not apply here — these are harmless when unneeded and still cover an
    // app routed through Xwayland. Set on sway's env, forwarded to the exec'd app like the DBUS
    // address above; applied before spec.env so an explicit override still wins.
    for (k, v) in glass_sandbox_linux::software_render_env(spec) {
        cmd.env(k, v);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    fn invalid_component(prefix: &[u8]) -> OsString {
        let mut bytes = prefix.to_vec();
        bytes.push(0xff);
        OsString::from_vec(bytes)
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn position_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn rposition_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .rposition(|window| window == needle)
    }

    #[test]
    fn shell_quote_preserves_apostrophe_invalid_utf8_whitespace_and_newline() {
        let argument = OsString::from_vec(b"arg-'\xff space\nnext".to_vec());
        let mut command = b"printf '%s' ".to_vec();
        shell_quote(argument.as_os_str(), &mut command);
        let output = Command::new("sh")
            .arg("-c")
            .arg(OsString::from_vec(command))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, argument.as_bytes());
    }

    #[test]
    fn config_renderer_preserves_non_utf8_home_argument() {
        let home = OsString::from_vec(b"/home/user-\xff".to_vec());
        let argv = vec![
            OsString::from("bwrap"),
            OsString::from("--setenv"),
            OsString::from("HOME"),
            home.clone(),
            OsString::from("--"),
            OsString::from("app"),
        ];
        let config = render_sway_config(&argv);
        assert!(contains_bytes(&config, home.as_bytes()));
    }

    fn test_sway_config(
        spec: &AppSpec,
        runtime_dir: &Path,
        a11y_bind_dir: Option<&Path>,
    ) -> String {
        String::from_utf8(sway_config(spec, runtime_dir, a11y_bind_dir, None, &[]).unwrap())
            .unwrap()
    }

    #[test]
    fn sway_config_passes_the_inherited_status_fd_to_bwrap() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        let protected = temp.path().join("protected");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::create_dir(&protected).unwrap();
        let pipe = glass_sandbox_linux::BwrapStatusPipe::new().unwrap();
        let mut s = spec(&["/bin/app"]);
        s.sandbox = SandboxLevel::Default;

        let config = sway_config(
            &s,
            &runtime,
            None,
            Some(pipe.writer_fd()),
            &[ProtectedHostPath::directory(&protected)],
        )
        .unwrap();

        assert!(contains_bytes(&config, b"'--unshare-pid'"));
        assert!(contains_bytes(
            &config,
            format!("'--json-status-fd' '{}'", pipe.writer_fd()).as_bytes()
        ));
    }

    #[test]
    fn sway_command_passes_the_status_writer_through_a_spawned_child() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("sway-probe");
        std::fs::write(
            &probe,
            b"#!/bin/sh\npython3 -c 'import os; os.write(int(os.environ[\"TEST_STATUS_FD\"]), b\"{\\\"child-pid\\\":42}\\n\")'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = temp.path().join("sway.cfg");
        std::fs::write(&config, b"").unwrap();
        let runtime = temp.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        let pipe = glass_sandbox_linux::BwrapStatusPipe::new().unwrap();
        let mut s = spec(&["/bin/app"]);
        s.sandbox = SandboxLevel::Default;
        s.env
            .push(("TEST_STATUS_FD".into(), pipe.writer_fd().to_string()));

        let mut command = build_sway_command_with_status(
            &probe,
            &config,
            &s,
            &runtime,
            None,
            Some(pipe.writer_fd()),
        )
        .unwrap();
        let mut child = command.spawn().unwrap();
        let mut reader = pipe.into_reader();
        child.wait().unwrap();

        assert_eq!(reader.poll_child_pid().unwrap(), Some(42));
    }

    #[test]
    fn sandbox_config_threads_real_status_fd_and_places_masks_after_display_and_a11y_binds() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        let a11y = temp.path().join("a11y");
        let protected = temp.path().join("protected");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::create_dir(&a11y).unwrap();
        std::fs::create_dir(&protected).unwrap();
        let pipe = glass_sandbox_linux::BwrapStatusPipe::new().unwrap();
        let mut s = spec(&["/bin/app"]);
        s.sandbox = SandboxLevel::Default;
        let config = sway_config(
            &s,
            &runtime,
            Some(&a11y),
            Some(pipe.writer_fd()),
            &[ProtectedHostPath::directory(&protected)],
        )
        .unwrap();
        let status = position_bytes(&config, b"'--json-status-fd'").unwrap();
        let runtime_bind = rposition_bytes(&config, runtime.as_os_str().as_bytes()).unwrap();
        let a11y_bind = rposition_bytes(&config, a11y.as_os_str().as_bytes()).unwrap();
        let mask = rposition_bytes(&config, protected.as_os_str().as_bytes()).unwrap();
        assert!(contains_bytes(
            &config[status..],
            format!("'{}'", pipe.writer_fd()).as_bytes()
        ));
        assert!(mask > runtime_bind);
        assert!(mask > a11y_bind);
    }

    #[test]
    fn sandbox_config_preserves_non_utf8_path_bytes_in_binds_and_final_masks() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join(invalid_component(b"runtime-"));
        let cwd = temp.path().join(invalid_component(b"cwd-"));
        let a11y = temp.path().join(invalid_component(b"a11y-"));
        let protected_dir = temp.path().join(invalid_component(b"protected-dir-"));
        let protected_file = temp.path().join(invalid_component(b"protected-file-"));
        for dir in [&runtime, &cwd, &a11y, &protected_dir] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(&protected_file, b"lease").unwrap();

        let alias_dir = temp.path().join("protected-dir-�");
        let alias_file = temp.path().join("protected-file-�");
        std::fs::create_dir(&alias_dir).unwrap();
        std::fs::write(&alias_file, b"alias").unwrap();

        let mut s = spec(&["/bin/app"]);
        s.sandbox = SandboxLevel::Default;
        s.cwd = Some(cwd.clone());
        let config = sway_config(
            &s,
            &runtime,
            Some(&a11y),
            None,
            &[
                ProtectedHostPath::directory(&protected_dir),
                ProtectedHostPath::file(&protected_file),
            ],
        )
        .unwrap();
        for path in [&runtime, &cwd, &a11y, &protected_dir, &protected_file] {
            assert!(
                contains_bytes(&config, path.as_os_str().as_bytes()),
                "config changed path bytes for {path:?}"
            );
        }
        assert!(!contains_bytes(&config, alias_dir.as_os_str().as_bytes()));
        assert!(!contains_bytes(&config, alias_file.as_os_str().as_bytes()));
    }

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
        let cfg = test_sway_config(
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
        let cfg = test_sway_config(&s, std::path::Path::new("/run/glass-rt"), None);
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

    /// An Xwayland client under containment reaches the display through
    /// `/tmp/.X11-unix/X<n>`, which the sandbox's ephemeral `/tmp` tmpfs shadows. Without
    /// this bind only clients that fall back to the abstract socket can connect.
    #[test]
    fn sway_config_binds_the_x11_socket_dir_when_sandboxed() {
        use glass_core::SandboxLevel;
        let mut s = spec(&["glass-testapp"]);
        s.sandbox = SandboxLevel::Default;
        let cfg = test_sway_config(&s, std::path::Path::new("/run/glass-rt"), None);
        assert!(
            cfg.contains("'--ro-bind-try' '/tmp/.X11-unix' '/tmp/.X11-unix'"),
            "{cfg}"
        );
    }

    #[test]
    fn sway_config_exec_unwrapped_when_off() {
        let cfg = test_sway_config(&spec(&["app"]), std::path::Path::new("/run/glass-rt"), None);
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

    /// Effective value of `key` in the built command.
    fn env_of(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(key))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned())
    }

    fn sway_cmd(s: &AppSpec) -> Command {
        build_sway_command(
            std::path::Path::new("/usr/bin/sway"),
            std::path::Path::new("/tmp/cfg"),
            s,
            std::path::Path::new("/run/glass-rt"),
            None,
        )
    }

    #[test]
    fn build_sway_command_injects_software_render_defaults_under_sandbox() {
        let mut s = spec(&["app"]);
        s.sandbox = glass_core::SandboxLevel::Default;
        let cmd = sway_cmd(&s);
        assert_eq!(env_of(&cmd, "GSK_RENDERER").as_deref(), Some("cairo"));
        assert_eq!(env_of(&cmd, "QT_X11_NO_MITSHM").as_deref(), Some("1"));
        assert_eq!(
            env_of(&cmd, "QT_QUICK_BACKEND").as_deref(),
            Some("software")
        );
    }

    #[test]
    fn build_sway_command_omits_software_render_defaults_when_sandbox_off() {
        let s = spec(&["app"]); // sandbox: Off
        let cmd = sway_cmd(&s);
        assert_eq!(env_of(&cmd, "GSK_RENDERER"), None);
    }

    #[test]
    fn build_sway_command_lets_spec_env_override_software_render_default() {
        let mut s = spec(&["app"]);
        s.sandbox = glass_core::SandboxLevel::Default;
        s.env = vec![("GSK_RENDERER".into(), "gl".into())];
        let cmd = sway_cmd(&s);
        assert_eq!(env_of(&cmd, "GSK_RENDERER").as_deref(), Some("gl"));
        assert_eq!(
            env_of(&cmd, "QT_QUICK_BACKEND").as_deref(),
            Some("software")
        );
    }

    #[test]
    fn sway_config_binds_an_absolute_argument_path_dir() {
        // A launch target reached only through an ARGUMENT path (not run[0]) must be re-exposed:
        // the arg's directory is bound into the exec bwrap argv. Runs under the default
        // (display-less) `cargo test`, catching an `&args` mis-wire the #[ignore]d integration
        // test would otherwise be the only guard against.
        use glass_core::SandboxLevel;
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap(); // under /tmp, not $HOME
        let asset = dir.path().join("asset.bin");
        std::fs::write(&asset, b"").unwrap();
        let asset_s = asset.to_string_lossy();
        let mut s = spec(&["/bin/cat", asset_s.as_ref()]);
        s.sandbox = SandboxLevel::Default;
        let cfg = test_sway_config(&s, std::path::Path::new("/run/glass-rt"), None);
        let argdir = dir.path().canonicalize().unwrap();
        let needle = format!("'--ro-bind-try' '{d}' '{d}'", d = argdir.to_string_lossy());
        assert!(
            cfg.contains(&needle),
            "arg dir not bound into the exec bwrap:\nwant: {needle}\ngot:\n{cfg}"
        );
    }

    #[test]
    fn sway_config_defaults_cwd_to_current_dir_when_unset() {
        // With cwd: None and a sandbox, glass defaults the working directory to its OWN cwd, so the
        // exec bwrap must carry `--chdir <current_dir>` (and, unless current_dir is the ephemeral
        // HOME/tmp or an ancestor, a rw `--bind <current_dir> <current_dir>`). Both test and code
        // read current_dir() in-process, so it's a literal-value assertion.
        use glass_core::SandboxLevel;
        let cwd = std::env::current_dir().unwrap();
        let mut s = spec(&["/bin/app"]);
        s.sandbox = SandboxLevel::Default;
        s.cwd = None;
        let cfg = test_sway_config(&s, std::path::Path::new("/run/glass-rt"), None);
        let cwd_s = cwd.to_string_lossy();
        assert!(
            cfg.contains(&format!("'--chdir' '{cwd_s}'")),
            "expected --chdir {cwd_s} in exec:\n{cfg}"
        );
        let home = ephemeral_home();
        let home_c = std::path::Path::new(&home)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from(&home));
        let cwd_c = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
        let cwd_is_root_or_ancestor = [home_c.as_path(), std::path::Path::new("/tmp")]
            .iter()
            .any(|root| root.starts_with(&cwd_c));
        if !cwd_is_root_or_ancestor {
            assert!(
                cfg.contains(&format!("'--bind' '{cwd_s}' '{cwd_s}'")),
                "expected --bind {cwd_s} {cwd_s} in exec:\n{cfg}"
            );
        }
    }

    #[test]
    fn sway_config_binds_a11y_dir_when_sandboxed() {
        let mut s = spec(&["app"]);
        s.sandbox = glass_core::SandboxLevel::Default;
        let cfg = test_sway_config(
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
