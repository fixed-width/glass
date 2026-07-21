use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;

use glass_core::{AppSpec, SandboxLevel};
use glass_sandbox_linux::{ephemeral_home, wrap_argv, WrapOpts};

/// Build the launch command for `spec.run`, forcing `DISPLAY=<display>` (and, when `a11y`
/// is given, `DBUS_SESSION_BUS_ADDRESS=<addr>` + `XDG_RUNTIME_DIR=<dir>` so the child both
/// talks to, and resolves its AT-SPI bus within, the private a11y dir — never the host's
/// `/run/user/UID/at-spi/`) so the child renders on the backend's X server. Entries in
/// `spec.env` are applied after, so the caller can still override either deliberately.
///
/// When `spec.sandbox` is not `Off`, the command is wrapped in a `bwrap`
/// invocation so the launched process runs in a sandboxed user namespace.
/// The X11 socket dir (`/tmp/.X11-unix`) is re-exposed read-only inside the
/// namespace so the app can still connect to the display. When `a11y.dir`
/// is given, that directory (which holds the private session-bus and at-spi
/// sockets) is also re-exposed so a sandboxed app can reach the a11y bus.
pub fn build_command(spec: &AppSpec, display: &str, a11y: Option<glass_core::A11yBind>) -> Command {
    let dbus_addr = a11y.map(|a| a.addr);
    let a11y_bind_dir = a11y.map(|a| a.dir);
    let mut cmd = match spec.sandbox {
        SandboxLevel::Off => {
            let mut c = Command::new(&spec.run[0]);
            c.args(&spec.run[1..]);
            // Make the launched app its own process-group leader (pgid == pid)
            // so `stop_app` can reap the whole group, not just this one pid.
            c.process_group(0);
            c
        }
        level => {
            let prog = OsString::from(&spec.run[0]);
            let args: Vec<OsString> = spec.run[1..].iter().map(OsString::from).collect();
            // Default the working directory to glass's own cwd when the spec sets none, so a
            // contained launch with no `cwd` still gets `--chdir` + a guarded rw bind of that
            // directory (matching `sandbox:"off"`) and any relative launch token resolves against
            // it. Computed once and shared by both `launch_ro_binds` and `WrapOpts.cwd`.
            let effective_cwd = spec.cwd.clone().or_else(|| std::env::current_dir().ok());
            let home_os = ephemeral_home();
            // Always re-expose the X11 socket dir; also re-expose the launch target — the program
            // and any path token under $HOME or /tmp, which the ephemeral tmpfs shadows.
            let mut ro_binds = vec![std::path::PathBuf::from("/tmp/.X11-unix")];
            ro_binds.extend(glass_sandbox_linux::launch_ro_binds(
                &prog,
                &args,
                std::path::Path::new(&home_os),
                effective_cwd
                    .as_deref()
                    .unwrap_or_else(|| std::path::Path::new("/")),
            ));
            // Re-expose the private a11y bus dir (session-bus + at-spi sockets) so a sandboxed
            // app can reach the advertised unix:path= sockets, like the X11 socket above.
            if let Some(dir) = a11y_bind_dir {
                ro_binds.push(dir.to_path_buf());
            }
            let opts = WrapOpts {
                level,
                home: home_os,
                cwd: effective_cwd,
                ro_binds,
                rw_binds: vec![],
            };
            let argv = wrap_argv(&prog, &args, &opts);
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            // Group leader: for a sandboxed launch the leader is `bwrap`, which
            // is `--die-with-parent`-tied to the app, so the group reap covers
            // the whole tree.
            c.process_group(0);
            c
        }
    };
    cmd.env("DISPLAY", display);
    if let Some(addr) = dbus_addr {
        cmd.env("DBUS_SESSION_BUS_ADDRESS", addr);
    }
    if let Some(dir) = a11y_bind_dir {
        // Point the app's AT-SPI resolution at the private runtime dir (mirroring the
        // Wayland path). Without this the app inherits the host XDG_RUNTIME_DIR, so
        // accesskit/at-spi can fall back to the host's /run/user/UID/at-spi/bus_0 — which
        // may be wedged/unlinked — and accesskit_unix panics on the missing socket. The
        // private dir is re-exposed read-only into the sandbox above, so the same absolute
        // path resolves inside bwrap too. Keeps a11y resolution isolated AND on a live bus.
        cmd.env("XDG_RUNTIME_DIR", dir);
    }
    // Applied last so an explicit spec.env entry wins over the forced defaults.
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
    use std::ffi::OsStr;
    use std::path::PathBuf;

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
    fn sets_program_args_and_display() {
        let cmd = build_command(&spec(&["/bin/app", "--flag", "x"]), ":99", None);
        assert_eq!(cmd.get_program(), OsStr::new("/bin/app"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec![OsStr::new("--flag"), OsStr::new("x")]);
        let display = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("DISPLAY"))
            .and_then(|(_, v)| v);
        assert_eq!(display, Some(OsStr::new(":99")));
    }

    #[test]
    fn spec_env_can_override_display_and_cwd_is_applied() {
        let mut s = spec(&["app"]);
        s.env = vec![("DISPLAY".into(), ":7".into())];
        s.cwd = Some(PathBuf::from("/tmp"));
        let cmd = build_command(&s, ":99", None);
        // last DISPLAY env wins (spec.env applied after the forced default)
        let display = cmd
            .get_envs()
            .filter(|(k, _)| *k == OsStr::new("DISPLAY"))
            .last()
            .and_then(|(_, v)| v);
        assert_eq!(display, Some(OsStr::new(":7")));
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn dbus_addr_sets_session_bus_env() {
        let cmd = build_command(
            &spec(&["app"]),
            ":99",
            Some(glass_core::A11yBind {
                addr: "unix:path=/tmp/bus",
                dir: std::path::Path::new("/tmp/bus-dir"),
            }),
        );
        let addr = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("DBUS_SESSION_BUS_ADDRESS"))
            .and_then(|(_, v)| v);
        assert_eq!(addr, Some(OsStr::new("unix:path=/tmp/bus")));
    }

    #[test]
    fn spec_env_overrides_injected_dbus_addr() {
        let mut s = spec(&["app"]);
        s.env = vec![(
            "DBUS_SESSION_BUS_ADDRESS".into(),
            "unix:path=/tmp/override".into(),
        )];
        let cmd = build_command(
            &s,
            ":99",
            Some(glass_core::A11yBind {
                addr: "unix:path=/tmp/bus",
                dir: std::path::Path::new("/tmp/bus-dir"),
            }),
        );
        // Command stores env as a map: a later .env() for the same key replaces the earlier
        // one, so a spec.env entry overrides the injected default.
        let addr = cmd
            .get_envs()
            .filter(|(k, _)| *k == OsStr::new("DBUS_SESSION_BUS_ADDRESS"))
            .last()
            .and_then(|(_, v)| v);
        assert_eq!(addr, Some(OsStr::new("unix:path=/tmp/override")));
    }

    #[test]
    fn none_dbus_addr_leaves_session_bus_unset() {
        let cmd = build_command(&spec(&["app"]), ":9", None);
        assert!(
            !cmd.get_envs()
                .any(|(k, _)| k == OsStr::new("DBUS_SESSION_BUS_ADDRESS")),
            "DBUS_SESSION_BUS_ADDRESS must not be set when dbus_addr is None"
        );
    }

    #[test]
    fn a11y_pins_private_xdg_runtime_dir() {
        // The app must resolve AT-SPI within the private dir, not the host's /run/user/UID
        // (whose at-spi bus may be wedged → accesskit_unix panic). Mirrors the Wayland path.
        let cmd = build_command(
            &spec(&["app"]),
            ":9",
            Some(glass_core::A11yBind {
                addr: "unix:path=/tmp/glass-a11y-xyz/session-bus",
                dir: std::path::Path::new("/tmp/glass-a11y-xyz"),
            }),
        );
        let xrd = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("XDG_RUNTIME_DIR"))
            .and_then(|(_, v)| v);
        assert_eq!(xrd, Some(OsStr::new("/tmp/glass-a11y-xyz")));
    }

    #[test]
    fn no_a11y_leaves_xdg_runtime_dir_unset() {
        let cmd = build_command(&spec(&["app"]), ":9", None);
        assert!(
            !cmd.get_envs()
                .any(|(k, _)| k == OsStr::new("XDG_RUNTIME_DIR")),
            "without a11y, XDG_RUNTIME_DIR must be left untouched"
        );
    }

    #[test]
    fn build_command_wraps_in_bwrap_when_sandboxed() {
        use glass_core::SandboxLevel;
        let mut s = spec(&["/bin/app", "--flag"]);
        s.sandbox = SandboxLevel::Default;
        let cmd = build_command(&s, ":99", None);
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("bwrap"));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--unshare-user".to_string()));
        assert!(args
            .windows(3)
            .any(|w| w == ["--ro-bind-try", "/tmp/.X11-unix", "/tmp/.X11-unix"]));
        let dd = args.iter().position(|x| x == "--").unwrap();
        assert_eq!(&args[dd + 1..], &["/bin/app", "--flag"]);
        let disp = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("DISPLAY"))
            .and_then(|(_, v)| v);
        assert_eq!(disp, Some(std::ffi::OsStr::new(":99")));
    }

    #[test]
    fn build_command_binds_an_absolute_argument_path_dir() {
        // A launch target reached only through an ARGUMENT path (not run[0]) must be re-exposed:
        // the arg's directory is bound into the bwrap argv. This runs under the default
        // (display-less) `cargo test`, so it catches an `&args` mis-wire that the #[ignore]d
        // integration test would otherwise be the only guard against.
        use glass_core::SandboxLevel;
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap(); // under /tmp, not $HOME
        let asset = dir.path().join("asset.bin");
        std::fs::write(&asset, b"").unwrap();
        let asset_s = asset.to_string_lossy();
        let mut s = spec(&["/bin/cat", asset_s.as_ref()]);
        s.sandbox = SandboxLevel::Default;
        let cmd = build_command(&s, ":99", None);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let argdir = dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--ro-bind-try" && w[1] == argdir && w[2] == argdir),
            "arg dir {argdir} not bound into bwrap argv: {args:?}"
        );
    }

    #[test]
    fn sandboxed_run_ro_binds_the_a11y_dir() {
        let mut s = spec(&["app"]);
        s.sandbox = glass_core::SandboxLevel::Default;
        let dir = std::path::Path::new("/tmp/glass-a11y-xyz");
        let cmd = build_command(
            &s,
            ":9",
            Some(glass_core::A11yBind {
                addr: "unix:path=/tmp/glass-a11y-xyz/session-bus",
                dir,
            }),
        );
        let joined: String = std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("/tmp/glass-a11y-xyz"),
            "a11y dir not bound into bwrap argv:\n{joined}"
        );
    }
}
