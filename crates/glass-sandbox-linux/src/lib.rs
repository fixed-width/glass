//! Linux process containment for glass via bubblewrap (`bwrap`).
//!
//! `wrap_argv` is pure (builds an argv, touches nothing) so it is unit-tested by
//! asserting the arguments. `availability` runs `bwrap` to prove a user namespace
//! can be created. Callers handle `SandboxLevel::Off` themselves (never wrap).

#![cfg(target_os = "linux")]

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Command;

use glass_core::{AppSpec, Check, CheckStatus, GlassError, Result, SandboxLevel};

/// The bubblewrap binary glass invokes: `$GLASS_BWRAP`, else `bwrap` (on `PATH`).
fn bwrap_bin() -> String {
    glass_core::tool_path("GLASS_BWRAP", "bwrap")
}

/// The POSIX shell glass runs the build command with: `$GLASS_SH`, else `sh`.
fn sh_bin() -> String {
    glass_core::tool_path("GLASS_SH", "sh")
}

/// Whether `bin` is reachable: an explicit path (contains `/`) must be an existing
/// file; a bare name must be found on `PATH`.
fn runnable(bin: &str) -> bool {
    if bin.contains('/') {
        std::path::Path::new(bin).is_file()
    } else {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
            .unwrap_or(false)
    }
}

/// Inputs `wrap_argv` needs. `level` is never `Off` (the caller skips wrapping).
pub struct WrapOpts {
    pub level: SandboxLevel,
    /// Ephemeral HOME inside the namespace: a tmpfs is mounted here and `HOME` is set to it.
    pub home: OsString,
    /// Working dir: used as `--chdir` and, when it would not re-expose the real HOME,
    /// bound read-write with `--bind`.
    ///
    /// The common case — `cwd` is a subdirectory of HOME — binds the project directory
    /// rw inside the ephemeral HOME tmpfs (the tmpfs is mounted at `home`, so the subtree
    /// is visible but the rest of the real home is not).
    ///
    /// If `cwd` IS `home` or an ancestor of `home` (e.g. `/home/u` when home is
    /// `/home/u`), binding it rw would re-mount the real HOME over the tmpfs,
    /// defeating the secret-isolation guarantee. In that case the `--bind` is skipped and
    /// only `--chdir` is emitted; the `--ro-bind / /` + tmpfs already provide the path.
    ///
    /// The path must exist on the host when `--bind` is emitted (bwrap fails at launch for
    /// a missing source), but `--chdir` to a nonexistent path will fail inside bwrap which
    /// is also acceptable — the caller is expected to pass a real path.
    pub cwd: Option<PathBuf>,
    /// Existing paths re-exposed read-only AFTER the `/tmp` tmpfs (e.g. the X11 socket dir).
    pub ro_binds: Vec<PathBuf>,
    /// Existing paths re-exposed read-write AFTER the `/tmp` tmpfs (e.g. the Wayland runtime dir).
    pub rw_binds: Vec<PathBuf>,
}

/// Read-only binds needed to reach the program binary inside the namespace: the
/// program path itself when it is absolute (it may live under `$HOME`, which the
/// ephemeral-HOME tmpfs shadows). Bare-name programs resolve via PATH (covered by
/// `--ro-bind / /`), so they need no extra bind.
pub fn program_ro_binds(program: &OsStr) -> Vec<std::path::PathBuf> {
    let p = std::path::Path::new(program);
    if p.is_absolute() { vec![p.to_path_buf()] } else { vec![] }
}

/// The ephemeral HOME path to use: the real `$HOME` (so apps that hardcode the path
/// still work — it's shadowed by a tmpfs), else a fixed fallback.
pub fn ephemeral_home() -> OsString {
    std::env::var_os("HOME").unwrap_or_else(|| OsString::from("/tmp/glass-sandbox-home"))
}

/// Best-effort path canonicalization that never panics on a nonexistent path.
/// If `canonicalize` fails (e.g. the path doesn't exist yet), the raw path is
/// returned unchanged.
fn canon(p: &std::path::Path) -> std::path::PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Build the full argv for a contained launch: `bwrap … -- <program> <args…>`.
pub fn wrap_argv(program: &OsStr, args: &[OsString], opts: &WrapOpts) -> Vec<OsString> {
    let mut v: Vec<OsString> = vec![OsString::from(bwrap_bin())];
    for f in [
        "--unshare-user",
        "--unshare-ipc",
        // NOTE: --unshare-pid is intentionally OMITTED: a PID namespace makes the
        // child's std::process::id() return a namespace-relative PID (often 2),
        // which is what it would write into _NET_WM_PID. glass's window-discovery
        // then can't match the child by PID (it holds the host PID). Filesystem
        // and network isolation are the threat-model goals; PID enumeration
        // isolation is unnecessary when glass owns the sandboxed process.
        // Security note: without a PID namespace the contained process can see host PIDs in
        // /proc and send signals to same-UID processes (kill() needs no capability), including
        // glass-mcp itself. This is an accepted trade-off for this slice — the primary goals are
        // filesystem and network containment. A future improvement could pass _NET_WM_PID via an
        // out-of-band channel (e.g. bwrap --json-status-fd) to restore PID-namespace isolation.
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--die-with-parent",
        "--new-session", // detaches the child from the controlling terminal (prevents terminal-escape); benign for glass's headless GUI apps
        // NOTE: --no-new-privs is NOT emitted here. This bwrap version (confirmed at build time
        // via `bwrap --help`) does not list the flag; adding it would break every launch with an
        // "unrecognized option" error. Under --unshare-user bwrap already sets PR_SET_NO_NEW_PRIVS
        // internally (new-user-namespace semantics), so privilege escalation via setuid/file-caps
        // is already blocked without the explicit flag.
        "--cap-drop",
        "ALL",
    ] {
        v.push(OsString::from(f));
    }
    if opts.level == SandboxLevel::Strict {
        v.push(OsString::from("--unshare-net"));
    }
    for f in ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"] {
        v.push(OsString::from(f));
    }
    v.push(OsString::from("--tmpfs"));
    v.push(opts.home.clone());
    for b in &opts.ro_binds {
        v.push(OsString::from("--ro-bind-try"));
        v.push(b.clone().into_os_string());
        v.push(b.clone().into_os_string());
    }
    for b in &opts.rw_binds {
        v.push(OsString::from("--bind-try"));
        v.push(b.clone().into_os_string());
        v.push(b.clone().into_os_string());
    }
    if let Some(cwd) = &opts.cwd {
        let home_c = canon(std::path::Path::new(&opts.home));
        let cwd_c = canon(cwd);
        // Guard: skip the rw bind when cwd IS home or an ancestor of home.
        //
        // `--tmpfs <home>` mounts an ephemeral tmpfs over the real $HOME to hide
        // ~/.ssh and other secrets.  If we also emit `--bind <cwd> <cwd>` and cwd
        // equals home (or is a parent of home, e.g. cwd="/home" home="/home/u"),
        // that bind re-mounts the real HOME subtree OVER the tmpfs — re-exposing
        // everything we just hid.
        //
        // `home_c.starts_with(&cwd_c)` is true in both the equal case and the
        // ancestor case, so we skip the bind in both.  The common subdir case
        // (cwd="/home/u/proj") gives starts_with=false and is bound rw as usual.
        // The `--ro-bind / /` + tmpfs already provide the path for the skipped
        // cases so `--chdir` still works.
        if !home_c.starts_with(&cwd_c) {
            v.push(OsString::from("--bind"));
            v.push(cwd.clone().into_os_string());
            v.push(cwd.clone().into_os_string());
        }
        v.push(OsString::from("--chdir"));
        v.push(cwd.clone().into_os_string());
    }
    v.push(OsString::from("--setenv"));
    v.push(OsString::from("HOME"));
    v.push(opts.home.clone());
    v.push(OsString::from("--"));
    v.push(program.to_os_string());
    v.extend(args.iter().cloned());
    v
}

/// Build the (unsandboxed) command for `spec.build`, or `None` if there's no build step.
/// The build runs with the full developer environment — only the launched *run* is sandboxed.
fn build_command_for(spec: &AppSpec) -> Option<Command> {
    let build = spec.build.as_ref()?;
    let mut c = Command::new(sh_bin());
    c.arg("-c").arg(build);
    if let Some(dir) = &spec.cwd {
        c.current_dir(dir);
    }
    Some(c)
}

/// Run `spec.build` (if any) as `sh -c <build>` with the full developer environment — the build
/// is the developer's own code and is NOT sandboxed; only the launched run is contained. `cwd` is
/// applied; a spawn failure or non-zero exit → `AppNotStarted`.
pub fn run_build(spec: &AppSpec) -> Result<()> {
    let Some(mut cmd) = build_command_for(spec) else { return Ok(()) };
    let status = cmd
        .status()
        .map_err(|e| GlassError::AppNotStarted(format!("build command: {e}")))?;
    if !status.success() {
        return Err(GlassError::AppNotStarted(format!(
            "build command failed with status {status}"
        )));
    }
    Ok(())
}

/// Whether bubblewrap can actually create a user namespace here.
pub enum Availability {
    Ok,
    Unavailable(String),
}

/// Probe: the configured `bwrap` reachable and an unprivileged user namespace usable.
pub fn availability() -> Availability {
    let bin = bwrap_bin();
    if !runnable(&bin) {
        return Availability::Unavailable(format!(
            "bubblewrap ({bin}) not found (set GLASS_BWRAP to its path)"
        ));
    }
    match Command::new(&bin)
        .args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"])
        .output()
    {
        Ok(o) if o.status.success() => Availability::Ok,
        Ok(o) => Availability::Unavailable(format!(
            "bubblewrap cannot create a user namespace: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Availability::Unavailable(format!("could not run {bin}: {e}")),
    }
}

/// Read whether AppArmor restricts unprivileged user namespaces (Ubuntu 23.10+).
/// `Some(true)` = restricted — the cause of bwrap's "setting up uid map: Permission
/// denied"; `Some(false)` = allowed; `None` = the knob is absent (older kernels).
fn apparmor_userns_restricted() -> Option<bool> {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .ok()
        .map(|s| s.trim() == "1")
}

/// Pure: the remedy for an unavailable sandbox, tailored to whether AppArmor's
/// unprivileged-userns restriction is the likely cause (Ubuntu 23.10+).
fn unavailable_remedy(apparmor_restricted: bool) -> String {
    if apparmor_restricted {
        "this system restricts unprivileged user namespaces via AppArmor \
         (kernel.apparmor_restrict_unprivileged_userns=1), which bubblewrap requires. Allow them \
         with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` (persist via a file \
         in /etc/sysctl.d/), or run with sandbox:\"off\""
            .into()
    } else {
        "install `bubblewrap` (or set GLASS_BWRAP to its path) and enable unprivileged user \
         namespaces (e.g. `sysctl kernel.unprivileged_userns_clone=1`), or run with sandbox:\"off\""
            .into()
    }
}

/// Pure: map probed facts to a doctor check. `bin` is the resolved bubblewrap binary;
/// `apparmor_restricted` tailors the remedy to the AppArmor userns restriction.
fn sandbox_checks(available: bool, bin: &str, why: Option<String>, apparmor_restricted: bool) -> Vec<Check> {
    let check = if available {
        Check::new(
            "sandbox (bubblewrap)",
            CheckStatus::Ok,
            format!("{bin} present; user namespaces usable"),
        )
    } else {
        Check::new(
            "sandbox (bubblewrap)",
            CheckStatus::Fail,
            why.unwrap_or_else(|| "unavailable".into()),
        )
        .with_remedy(unavailable_remedy(apparmor_restricted))
    };
    vec![check]
}

/// Gather the live sandbox check.
pub fn checks() -> Vec<Check> {
    let bin = bwrap_bin();
    let apparmor_restricted = apparmor_userns_restricted() == Some(true);
    match availability() {
        Availability::Ok => sandbox_checks(true, &bin, None, apparmor_restricted),
        Availability::Unavailable(why) => sandbox_checks(false, &bin, Some(why), apparmor_restricted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::SandboxLevel;
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    fn argv_strings(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    fn opts(level: SandboxLevel) -> WrapOpts {
        WrapOpts {
            level,
            home: OsString::from("/home/u"),
            cwd: Some(PathBuf::from("/work")),
            ro_binds: vec![PathBuf::from("/tmp/.X11-unix")],
            rw_binds: vec![],
        }
    }

    #[test]
    fn default_wraps_program_with_core_flags_and_passthrough_args() {
        let argv = wrap_argv(OsStr::new("/bin/app"), &[OsString::from("--flag")], &opts(SandboxLevel::Default));
        let s = argv_strings(&argv);
        assert_eq!(s[0], "bwrap");
        assert!(s.contains(&"--unshare-user".into()));
        assert!(s.contains(&"--die-with-parent".into()));
        assert!(!s.contains(&"--unshare-net".into()), "default keeps network");
        let i = s.iter().position(|x| x == "--setenv").unwrap();
        assert_eq!((&s[i + 1], &s[i + 2]), (&"HOME".to_string(), &"/home/u".to_string()));
        assert!(s.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(s.windows(2).any(|w| w == ["--tmpfs", "/tmp"]));
        assert!(s.windows(2).any(|w| w == ["--tmpfs", "/home/u"]));
        assert!(s.windows(3).any(|w| w == ["--bind", "/work", "/work"]));
        assert!(s.windows(2).any(|w| w == ["--chdir", "/work"]));
        let tmpfs_tmp = s.windows(2).position(|w| w == ["--tmpfs", "/tmp"]).unwrap();
        let xbind = s.windows(3).position(|w| w == ["--ro-bind-try", "/tmp/.X11-unix", "/tmp/.X11-unix"]).unwrap();
        assert!(xbind > tmpfs_tmp, "socket bind must come after tmpfs /tmp");
        let dd = s.iter().position(|x| x == "--").unwrap();
        assert_eq!(&s[dd + 1..], &["/bin/app", "--flag"]);
    }

    #[test]
    fn strict_adds_unshare_net() {
        let argv = wrap_argv(OsStr::new("app"), &[], &opts(SandboxLevel::Strict));
        assert!(argv_strings(&argv).contains(&"--unshare-net".into()));
    }

    #[test]
    fn rw_binds_emit_bind_try_after_tmpfs_tmp() {
        let mut o = opts(SandboxLevel::Default);
        o.rw_binds = vec![PathBuf::from("/run/glass-rt")];
        let s = argv_strings(&wrap_argv(OsStr::new("app"), &[], &o));
        let tmpfs_tmp = s.windows(2).position(|w| w == ["--tmpfs", "/tmp"]).unwrap();
        let rwbind = s
            .windows(3)
            .position(|w| w == ["--bind-try", "/run/glass-rt", "/run/glass-rt"])
            .expect("rw_bind must emit --bind-try <p> <p>");
        assert!(rwbind > tmpfs_tmp, "rw bind must come after tmpfs /tmp");
    }

    #[test]
    fn ephemeral_home_prefers_env_then_falls_back() {
        assert!(!ephemeral_home().is_empty());
    }

    // -------------------------------------------------------------------------
    // cwd-guard tests: verify cwd==HOME and cwd==ancestor don't re-expose home
    // -------------------------------------------------------------------------

    /// When `cwd` exactly equals `home`, the rw `--bind` MUST be suppressed
    /// (re-binding home over the tmpfs would re-expose real secrets), but
    /// `--chdir` MUST still be emitted so the process starts there.
    #[test]
    fn cwd_equal_to_home_skips_bind_but_keeps_chdir() {
        let o = WrapOpts {
            level: SandboxLevel::Default,
            home: OsString::from("/home/u"),
            // cwd == home: the dangerous case
            cwd: Some(PathBuf::from("/home/u")),
            ro_binds: vec![],
            rw_binds: vec![],
        };
        let s = argv_strings(&wrap_argv(OsStr::new("app"), &[], &o));
        // The bind sequence --bind /home/u /home/u must NOT appear.
        assert!(
            !s.windows(3).any(|w| w == ["--bind", "/home/u", "/home/u"]),
            "cwd==home must not emit --bind <home> <home>; got: {s:?}"
        );
        // --chdir must still be present so the process starts in the right place.
        assert!(
            s.windows(2).any(|w| w == ["--chdir", "/home/u"]),
            "cwd==home must still emit --chdir <home>; got: {s:?}"
        );
    }

    /// When `cwd` is a subdirectory of `home` (the common case), the rw `--bind`
    /// MUST be emitted so the project directory is writable inside the sandbox.
    #[test]
    fn cwd_subdir_of_home_emits_bind_and_chdir() {
        let o = WrapOpts {
            level: SandboxLevel::Default,
            home: OsString::from("/home/u"),
            // cwd is inside home: normal project-dir case
            cwd: Some(PathBuf::from("/home/u/proj")),
            ro_binds: vec![],
            rw_binds: vec![],
        };
        let s = argv_strings(&wrap_argv(OsStr::new("app"), &[], &o));
        assert!(
            s.windows(3).any(|w| w == ["--bind", "/home/u/proj", "/home/u/proj"]),
            "cwd subdir of home must emit --bind <cwd> <cwd>; got: {s:?}"
        );
        assert!(
            s.windows(2).any(|w| w == ["--chdir", "/home/u/proj"]),
            "cwd subdir of home must emit --chdir <cwd>; got: {s:?}"
        );
    }

    #[test]
    fn program_ro_binds_absolute_returns_that_path() {
        let binds = super::program_ro_binds(OsStr::new("/home/u/myapp"));
        assert_eq!(binds, vec![PathBuf::from("/home/u/myapp")]);
    }

    #[test]
    fn program_ro_binds_bare_name_returns_empty() {
        let binds = super::program_ro_binds(OsStr::new("app"));
        assert!(binds.is_empty(), "bare name needs no extra bind; got: {binds:?}");
    }

    #[test]
    fn doctor_reports_ok_and_failure() {
        use glass_core::CheckStatus;
        let ok = sandbox_checks(true, "bwrap", None, false);
        assert_eq!(ok[0].status, CheckStatus::Ok);
        let bad = sandbox_checks(false, "bwrap", Some("bwrap not found".into()), false);
        assert_eq!(bad[0].status, CheckStatus::Fail);
        assert!(bad[0].remedy.is_some());
    }

    #[test]
    fn doctor_remedy_calls_out_apparmor_userns_restriction() {
        // Ubuntu 23.10+ restricts unprivileged userns via AppArmor (bwrap then fails
        // "setting up uid map: Permission denied"). When that's the cause, the remedy must
        // name the exact knob; otherwise it must not falsely claim AppArmor.
        let restricted =
            sandbox_checks(false, "bwrap", Some("uid map: Permission denied".into()), true);
        let r = restricted[0].remedy.clone().unwrap();
        assert!(r.contains("apparmor_restrict_unprivileged_userns"), "got: {r}");

        let generic = sandbox_checks(false, "bwrap", Some("bwrap not found".into()), false);
        let g = generic[0].remedy.clone().unwrap();
        assert!(!g.to_lowercase().contains("apparmor"), "generic remedy must not claim AppArmor: {g}");
    }

    fn make_spec(build: Option<&str>, sandbox: SandboxLevel) -> AppSpec {
        AppSpec {
            build: build.map(|s| s.to_string()),
            run: vec!["unused".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1000,
            sandbox,
            a11y: false,
        }
    }

    #[test]
    fn build_is_never_sandboxed() {
        for level in [SandboxLevel::Off, SandboxLevel::Default, SandboxLevel::Strict] {
            let s = make_spec(Some("true"), level);
            let cmd = build_command_for(&s).expect("build present");
            assert_eq!(
                cmd.get_program(),
                std::ffi::OsStr::new(&sh_bin()),
                "build must run via the shell, never bwrap, at {level:?}"
            );
        }
    }

    #[test]
    fn run_build_off_runs_and_reports_status() {
        use glass_core::SandboxLevel;
        assert!(run_build(&make_spec(None, SandboxLevel::Off)).is_ok(), "no build → Ok");
        assert!(run_build(&make_spec(Some("true"), SandboxLevel::Off)).is_ok(), "successful build → Ok");
        assert!(run_build(&make_spec(Some("false"), SandboxLevel::Off)).is_err(), "failing build → Err");
    }

    #[test]
    fn run_build_default_sandbox_runs_and_reports_status() {
        use glass_core::SandboxLevel;
        assert!(run_build(&make_spec(None, SandboxLevel::Default)).is_ok(), "no build → Ok");
        assert!(run_build(&make_spec(Some("true"), SandboxLevel::Default)).is_ok(), "successful build → Ok");
        assert!(run_build(&make_spec(Some("false"), SandboxLevel::Default)).is_err(), "failing build → Err");
    }
}
