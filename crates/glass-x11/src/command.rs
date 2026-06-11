use std::ffi::OsString;
use std::process::Command;

use glass_core::{AppSpec, SandboxLevel};
use glass_sandbox_linux::{ephemeral_home, wrap_argv, WrapOpts};

/// Build the launch command for `spec.run`, forcing `DISPLAY=<display>` (and, when
/// `dbus_addr` is given, `DBUS_SESSION_BUS_ADDRESS=<addr>` so the child talks to the
/// private a11y bus) so the child renders on the backend's X server. Entries in
/// `spec.env` are applied after, so the caller can still override either deliberately.
///
/// When `spec.sandbox` is not `Off`, the command is wrapped in a `bwrap`
/// invocation so the launched process runs in a sandboxed user namespace.
/// The X11 socket dir (`/tmp/.X11-unix`) is re-exposed read-only inside the
/// namespace so the app can still connect to the display.
pub fn build_command(spec: &AppSpec, display: &str, dbus_addr: Option<&str>) -> Command {
    let mut cmd = match spec.sandbox {
        SandboxLevel::Off => {
            let mut c = Command::new(&spec.run[0]);
            c.args(&spec.run[1..]);
            c
        }
        level => {
            let prog = OsString::from(&spec.run[0]);
            let args: Vec<OsString> = spec.run[1..].iter().map(OsString::from).collect();
            // Always re-expose the X11 socket dir; also re-expose the program
            // binary itself when it is absolute (it may live under $HOME, which
            // the ephemeral tmpfs shadows). PATH-resolved bare names are covered
            // by `--ro-bind / /` and need no extra bind.
            let mut ro_binds = vec![std::path::PathBuf::from("/tmp/.X11-unix")];
            ro_binds.extend(glass_sandbox_linux::program_ro_binds(&prog));
            let opts = WrapOpts {
                level,
                home: ephemeral_home(),
                cwd: spec.cwd.clone(),
                ro_binds,
                rw_binds: vec![],
            };
            let argv = wrap_argv(&prog, &args, &opts);
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        }
    };
    cmd.env("DISPLAY", display);
    if let Some(addr) = dbus_addr {
        cmd.env("DBUS_SESSION_BUS_ADDRESS", addr);
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
        let cmd = build_command(&spec(&["app"]), ":99", Some("unix:path=/tmp/bus"));
        let addr = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("DBUS_SESSION_BUS_ADDRESS"))
            .and_then(|(_, v)| v);
        assert_eq!(addr, Some(OsStr::new("unix:path=/tmp/bus")));
    }

    #[test]
    fn spec_env_overrides_injected_dbus_addr() {
        let mut s = spec(&["app"]);
        s.env = vec![("DBUS_SESSION_BUS_ADDRESS".into(), "unix:path=/tmp/override".into())];
        let cmd = build_command(&s, ":99", Some("unix:path=/tmp/bus"));
        // spec.env is applied after the forced default, so the explicit entry wins.
        let addr = cmd
            .get_envs()
            .filter(|(k, _)| *k == OsStr::new("DBUS_SESSION_BUS_ADDRESS"))
            .last()
            .and_then(|(_, v)| v);
        assert_eq!(addr, Some(OsStr::new("unix:path=/tmp/override")));
    }

    #[test]
    fn build_command_wraps_in_bwrap_when_sandboxed() {
        use glass_core::SandboxLevel;
        let mut s = spec(&["/bin/app", "--flag"]);
        s.sandbox = SandboxLevel::Default;
        let cmd = build_command(&s, ":99", None);
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("bwrap"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.contains(&"--unshare-user".to_string()));
        assert!(args.windows(3).any(|w| w == ["--ro-bind-try", "/tmp/.X11-unix", "/tmp/.X11-unix"]));
        let dd = args.iter().position(|x| x == "--").unwrap();
        assert_eq!(&args[dd + 1..], &["/bin/app", "--flag"]);
        let disp = cmd.get_envs().find(|(k, _)| *k == std::ffi::OsStr::new("DISPLAY")).and_then(|(_, v)| v);
        assert_eq!(disp, Some(std::ffi::OsStr::new(":99")));
    }
}
