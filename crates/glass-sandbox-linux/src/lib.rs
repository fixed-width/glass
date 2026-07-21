//! Linux process containment for glass via bubblewrap (`bwrap`).
//!
//! `wrap_argv` is pure (builds an argv, touches nothing) so it is unit-tested by
//! asserting the arguments. `availability` runs `bwrap` to prove a user namespace
//! can be created. Callers handle `SandboxLevel::Off` themselves (never wrap).

#![cfg(target_os = "linux")]

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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

/// Read-only binds that make the LITERAL launch target reachable inside the namespace: the
/// program and every `run` argument that resolves to an existing path. The ephemeral-`$HOME` and
/// `/tmp` tmpfs shadow anything under those roots, so a script/asset/binary living there must be
/// re-bound to be visible.
///
/// Three resolution rules mirror how the child is actually exec'd, so the paths bwrap opens match
/// the paths the launch touches:
/// - **`run[0]` as a bare name** (no `/`) is resolved against `$PATH` like `execvp`; the resolving
///   directory is bound only when it is under a shadowed root (e.g. `~/.cargo/bin`, an asdf shim).
///   A match under `/usr/bin` etc. is already visible via `--ro-bind / /`, so it contributes
///   nothing. Arguments are NOT `$PATH`-resolved.
/// - **A relative token** (program-with-`/` or any relative argument) is resolved against `cwd`
///   (`cwd.join(token)`) before binding, so `./start.sh` / `sub/asset` reach their real location.
///   When `cwd` is `None` (glass could not resolve a default working directory) a relative token
///   is SKIPPED rather than resolved against a wrong root.
/// - **An absolute token** is bound as-is.
///
/// For each resolved token, bwrap opens the path *as written*, but a symlink's target may live
/// elsewhere, so BOTH the literal path's directory (so the symlink/file is readable where it is
/// named) and the resolved target's directory (so the target is readable) are exposed — deduped,
/// so a non-symlink collapses to a single bind. Each directory is exposed EXCEPT when it is a
/// tmpfs-shadowed root (`home` or `/tmp`) or an ancestor of one — binding it would re-mount the
/// real subtree over the tmpfs, so only the file itself is bound. A target that IS itself a
/// shadowed root or an ancestor of one (e.g. an arg of `/tmp`, `home`, or `/`) contributes no bind
/// at all, for the same reason. Read-only, de-duplicated. Mirrors the `cwd` guard in `WrapOpts`.
pub fn launch_ro_binds(
    program: &OsStr,
    args: &[OsString],
    home: &Path,
    cwd: Option<&Path>,
) -> Vec<PathBuf> {
    let home = canon(home);
    let shadowed_roots = [home.as_path(), Path::new("/tmp")];
    let mut out: Vec<PathBuf> = Vec::new();

    // run[0] (the program).
    if program.as_bytes().contains(&b'/') {
        // A path program: absolute, or relative (e.g. `./start.sh`) → resolved against cwd. A
        // relative token with no known cwd resolves to nothing and is skipped (not bound to `/`).
        if let Some(p) = abs_token(Path::new(program), cwd) {
            push_token_binds(&mut out, &p, &shadowed_roots);
        }
    } else if let Some(resolved) = resolve_on_path(program) {
        // A bare name → resolve via `$PATH` like execvp. Bind the resolving directory ONLY when
        // it is under a shadowed root (a `$HOME`/`/tmp` PATH dir such as `~/.cargo/bin`, an asdf
        // shim); a match under `/usr/bin` etc. is already visible via `--ro-bind / /`.
        let dir = canon(resolved.parent().unwrap_or(&resolved));
        if shadowed_roots.iter().any(|root| dir.starts_with(root)) {
            push_token_binds(&mut out, &resolved, &shadowed_roots);
        }
    }

    // run[1..] (arguments): absolute or cwd-relative path tokens (never $PATH-resolved).
    for a in args {
        if let Some(p) = abs_token(Path::new(a), cwd) {
            push_token_binds(&mut out, &p, &shadowed_roots);
        }
    }
    out
}

/// Resolve a token to an absolute host path: an absolute token as-is, a relative one against
/// `cwd` (`execvp`/shell semantics), so a relative launch argument reaches its real location.
/// Returns `None` for a relative token when `cwd` is unknown — the token is then skipped rather
/// than resolved against a wrong root like `/`.
fn abs_token(tok: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    if tok.is_absolute() {
        Some(tok.to_path_buf())
    } else {
        cwd.map(|c| c.join(tok))
    }
}

/// The first `$PATH` entry holding an executable regular file named `program`, resolved the way
/// `execvp` resolves a bare command name. `None` when `$PATH` is unset or nothing matches. Mirrors
/// the `PATH` scan in [`runnable`], but returns the match (not just a bool) and requires an execute
/// bit so a same-named non-executable file is skipped like `execvp` skips it.
fn resolve_on_path(program: &OsStr) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    resolve_on_path_in(program, &path)
}

/// [`resolve_on_path`] against an explicit `$PATH` value — the testable seam (no global env).
fn resolve_on_path_in(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|cand| is_executable_file(cand))
}

/// Whether `p` is (or resolves through symlinks to) a regular file with at least one execute bit —
/// `execvp`'s "is this runnable" test.
fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Append the guarded read-only binds that make `lit` — an absolute launch-target path already
/// resolved against `cwd`/`$PATH` — reachable, de-duplicated into `out`.
///
/// bwrap opens the LITERAL path, but a symlink's target may live elsewhere, so BOTH the literal
/// path's directory and the resolved target's directory are exposed. The directory used for every
/// shadowed-root guard check is CANONICALIZED so a `..` component cannot sneak a shadowed root past
/// the guard.
fn push_token_binds(out: &mut Vec<PathBuf>, lit: &Path, roots: &[&Path]) {
    // `metadata` follows symlinks. ANY stat error — NotFound, EACCES, a dangling symlink, ELOOP,
    // … — is DELIBERATELY treated as "not a bindable path" and skipped: a token we cannot even
    // stat (a flag, a value, a missing file) contributes no bind. This is the fail-safe — we never
    // bind something we cannot confirm exists.
    if std::fs::metadata(lit).is_err() {
        return;
    }
    let real = canon(lit); // the resolved target (symlinks followed)
                           // Never auto-expose a shadowed root itself (or an ancestor of one) — binding it would re-mount
                           // the real subtree over the tmpfs. Such a target needs cwd / sandbox off.
    if roots.iter().any(|root| root.starts_with(&real)) {
        return;
    }
    // The directory to expose for a path: the path itself when it is a directory, else its parent.
    // Canonicalized so the guard checks below see a `..`-free path.
    let dir_of = |p: &Path| -> PathBuf {
        if p.is_dir() {
            canon(p)
        } else {
            canon(p.parent().unwrap_or(p))
        }
    };
    // Where the token is WRITTEN (so a symlink is readable at its literal location) AND where its
    // target actually LIVES. These coincide for a non-symlink (or same-dir symlink), so dedup
    // collapses them to one bind.
    for dir in [dir_of(lit), dir_of(&real)] {
        // A shadowed-root (or ancestor) directory must never be bound as a directory; the target
        // is a genuine file/subpath under it (checked above), so bind just the file.
        let bind = if roots.iter().any(|root| root.starts_with(&dir)) {
            real.clone()
        } else {
            dir
        };
        if !out.contains(&bind) {
            out.push(bind);
        }
    }
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
    for f in [
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ] {
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
        // Guard: skip the rw bind when cwd IS a tmpfs-shadowed root (`home` or `/tmp`) or an
        // ancestor of one. Mirrors the `shadowed_roots` prefix logic in `launch_ro_binds`.
        //
        // `--tmpfs <home>` and `--tmpfs /tmp` mount ephemeral tmpfs over the real $HOME (hiding
        // ~/.ssh etc.) and /tmp. If we also emit `--bind <cwd> <cwd>` and cwd equals a shadowed
        // root (or is a parent of one — e.g. cwd="/home" with home="/home/u", or cwd="/tmp" now
        // that cwd defaults to glass's current dir), that bind re-mounts the real subtree OVER the
        // tmpfs, re-exposing everything we just hid.
        //
        // `root.starts_with(&cwd_c)` is true when cwd equals a root or is an ancestor of it, so we
        // skip the bind in both cases. The common subdir case (cwd="/home/u/proj" or "/tmp/scratch")
        // gives false and is bound rw as usual. The `--ro-bind / /` + tmpfs already provide the path
        // for the skipped cases so `--chdir` still works.
        let shadowed_roots = [home_c.as_path(), std::path::Path::new("/tmp")];
        if !shadowed_roots.iter().any(|root| root.starts_with(&cwd_c)) {
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
    let Some(mut cmd) = build_command_for(spec) else {
        return Ok(());
    };
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
fn sandbox_checks(
    available: bool,
    bin: &str,
    why: Option<String>,
    apparmor_restricted: bool,
) -> Vec<Check> {
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
        Availability::Unavailable(why) => {
            sandbox_checks(false, &bin, Some(why), apparmor_restricted)
        }
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
        let argv = wrap_argv(
            OsStr::new("/bin/app"),
            &[OsString::from("--flag")],
            &opts(SandboxLevel::Default),
        );
        let s = argv_strings(&argv);
        assert_eq!(s[0], "bwrap");
        assert!(s.contains(&"--unshare-user".into()));
        assert!(s.contains(&"--die-with-parent".into()));
        assert!(
            !s.contains(&"--unshare-net".into()),
            "default keeps network"
        );
        let i = s.iter().position(|x| x == "--setenv").unwrap();
        assert_eq!(
            (&s[i + 1], &s[i + 2]),
            (&"HOME".to_string(), &"/home/u".to_string())
        );
        assert!(s.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(s.windows(2).any(|w| w == ["--tmpfs", "/tmp"]));
        assert!(s.windows(2).any(|w| w == ["--tmpfs", "/home/u"]));
        assert!(s.windows(3).any(|w| w == ["--bind", "/work", "/work"]));
        assert!(s.windows(2).any(|w| w == ["--chdir", "/work"]));
        let tmpfs_tmp = s.windows(2).position(|w| w == ["--tmpfs", "/tmp"]).unwrap();
        let xbind = s
            .windows(3)
            .position(|w| w == ["--ro-bind-try", "/tmp/.X11-unix", "/tmp/.X11-unix"])
            .unwrap();
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
            s.windows(3)
                .any(|w| w == ["--bind", "/home/u/proj", "/home/u/proj"]),
            "cwd subdir of home must emit --bind <cwd> <cwd>; got: {s:?}"
        );
        assert!(
            s.windows(2).any(|w| w == ["--chdir", "/home/u/proj"]),
            "cwd subdir of home must emit --chdir <cwd>; got: {s:?}"
        );
    }

    /// `/tmp` is a tmpfs-shadowed root too. Now that cwd defaults to glass's own working
    /// directory, glass running with cwd exactly `/tmp` must NOT emit `--bind /tmp /tmp` (that
    /// would re-mount host `/tmp` over the ephemeral tmpfs), but `--chdir /tmp` must still appear.
    #[test]
    fn cwd_equal_to_tmp_skips_bind_but_keeps_chdir() {
        let o = WrapOpts {
            level: SandboxLevel::Default,
            home: OsString::from("/home/u"),
            // cwd == /tmp: the dangerous case the home-only guard used to miss.
            cwd: Some(PathBuf::from("/tmp")),
            ro_binds: vec![],
            rw_binds: vec![],
        };
        let s = argv_strings(&wrap_argv(OsStr::new("app"), &[], &o));
        assert!(
            !s.windows(3).any(|w| w == ["--bind", "/tmp", "/tmp"]),
            "cwd==/tmp must not emit --bind /tmp /tmp; got: {s:?}"
        );
        assert!(
            s.windows(2).any(|w| w == ["--chdir", "/tmp"]),
            "cwd==/tmp must still emit --chdir /tmp; got: {s:?}"
        );
    }

    // -------------------------------------------------------------------------
    // launch_ro_binds tests
    // -------------------------------------------------------------------------

    /// `launch_ro_binds` with a throwaway EMPTY `cwd`, so no relative token resolves against it —
    /// for the cases that exercise only bare-name/absolute tokens. Cases that test cwd-relative
    /// resolution call `launch_ro_binds` directly with a populated `cwd`.
    fn ro_binds(program: &OsStr, args: &[OsString], home: &Path) -> Vec<PathBuf> {
        let cwd = tempfile::tempdir().unwrap();
        launch_ro_binds(program, args, home, Some(cwd.path()))
    }

    #[test]
    fn bare_name_program_via_usr_bin_binds_nothing() {
        // `env` is a coreutils tool guaranteed on the system PATH under a non-shadowed dir
        // (/usr/bin, already visible via --ro-bind / /), so resolving it contributes no bind. Using
        // `env` rather than `python3` guarantees the "resolves under a non-shadowed dir → no bind"
        // branch is actually exercised (a missing program would take the None path instead).
        let home = tempfile::tempdir().unwrap();
        assert!(ro_binds(OsStr::new("env"), &[], home.path()).is_empty());
    }

    #[test]
    fn program_under_a_project_dir_binds_its_directory() {
        let home = tempfile::tempdir().unwrap();
        let proj = home.path().join("proj/app");
        std::fs::create_dir_all(&proj).unwrap();
        let bin = proj.join("bin");
        std::fs::write(&bin, b"").unwrap();
        let out = ro_binds(bin.as_os_str(), &[], home.path());
        assert_eq!(out, vec![proj.canonicalize().unwrap()]); // the file's directory, not the file
    }

    #[test]
    fn arg_script_binds_its_directory_so_siblings_are_reachable() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("app.py");
        std::fs::write(&script, b"").unwrap();
        let out = ro_binds(
            OsStr::new("python3"),
            &[OsString::from(&script)],
            home.path(),
        );
        assert_eq!(out, vec![dir.canonicalize().unwrap()]);
    }

    #[test]
    fn existing_directory_arg_binds_itself() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("proj/data");
        std::fs::create_dir_all(&data).unwrap();
        let out = ro_binds(
            OsStr::new("srv"),
            &[OsString::from("--root"), OsString::from(&data)],
            home.path(),
        );
        assert_eq!(out, vec![data.canonicalize().unwrap()]);
    }

    #[test]
    fn target_directly_in_home_binds_only_the_file() {
        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("app.py"); // parent dir == home
        std::fs::write(&script, b"").unwrap();
        let out = ro_binds(
            OsStr::new("python3"),
            &[OsString::from(&script)],
            home.path(),
        );
        assert_eq!(out, vec![script.canonicalize().unwrap()]); // guard: never bind home itself as a dir
        assert!(!out
            .iter()
            .any(|p| *p == home.path().canonicalize().unwrap()));
    }

    #[test]
    fn nonexistent_tokens_contribute_nothing() {
        // Flags/values and missing paths — bare, relative, or absolute — are not bindable. With an
        // empty cwd the relative tokens (`http.server`, `app.py`) resolve to nothing, so the whole
        // launch yields no binds.
        let home = tempfile::tempdir().unwrap();
        let out = ro_binds(
            OsStr::new("python3"),
            &[
                OsString::from("-m"),
                OsString::from("http.server"),
                OsString::from("app.py"),
                OsString::from("/no/such/abs/path"),
            ],
            home.path(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn duplicate_dirs_are_collapsed() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.py");
        std::fs::write(&a, b"").unwrap();
        let b = dir.join("b.py");
        std::fs::write(&b, b"").unwrap();
        let out = ro_binds(
            OsStr::new("python3"),
            &[OsString::from(&a), OsString::from(&b)],
            home.path(),
        );
        assert_eq!(out, vec![dir.canonicalize().unwrap()]);
    }

    #[test]
    fn arg_equal_to_tmp_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let out = ro_binds(
            OsStr::new("python3"),
            &[OsString::from("/tmp")],
            home.path(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn arg_equal_to_home_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let out = ro_binds(
            OsStr::new("python3"),
            &[OsString::from(home.path())],
            home.path(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn ancestor_of_home_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("a/b/c");
        std::fs::create_dir_all(&home).unwrap();
        let ancestor = root.path().join("a/b"); // ancestor of home, not home itself
        let out = ro_binds(OsStr::new("python3"), &[OsString::from(&ancestor)], &home);
        assert!(out.is_empty());
    }

    #[test]
    fn file_directly_under_tmp_binds_the_file_only() {
        let home = tempfile::tempdir().unwrap(); // unrelated to the /tmp file below
        let file = tempfile::Builder::new().tempfile_in("/tmp").unwrap();
        let out = ro_binds(
            OsStr::new("python3"),
            &[OsString::from(file.path())],
            home.path(),
        );
        assert_eq!(out, vec![file.path().canonicalize().unwrap()]);
    }

    // --- literal-path (symlink) reachability -------------------------------------------------

    /// A symlink under `home` whose target lives OUTSIDE both shadowed roots (a venv/pyenv-style
    /// `bin/python` → a system binary): bwrap opens the LITERAL symlink, so its directory must be
    /// bound, not only the resolved target's. This is the `run[0]` regression the first increment
    /// re-introduced by deciding binds from `canonicalize()` alone.
    #[test]
    fn symlink_program_binds_the_literal_dir_even_when_target_is_outside_roots() {
        let home = tempfile::tempdir().unwrap();
        let bindir = home.path().join("venv/bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let target = Path::new("/bin/sh"); // a real binary outside home and /tmp
        assert!(target.exists(), "test needs /bin/sh present");
        let link = bindir.join("python");
        std::os::unix::fs::symlink(target, &link).unwrap();
        let out = ro_binds(link.as_os_str(), &[], home.path());
        assert!(
            out.contains(&bindir.canonicalize().unwrap()),
            "literal symlink's dir must be bound so bwrap can open it as written; got {out:?}"
        );
    }

    /// A symlink under `home` whose target ALSO lives under `home` (a different directory): BOTH the
    /// literal symlink's directory and the resolved target's directory must be bound.
    #[test]
    fn symlink_program_target_under_home_binds_both_dirs() {
        let home = tempfile::tempdir().unwrap();
        let bindir = home.path().join("venv/bin");
        let libdir = home.path().join("venv/lib");
        std::fs::create_dir_all(&bindir).unwrap();
        std::fs::create_dir_all(&libdir).unwrap();
        let target = libdir.join("python3.real");
        std::fs::write(&target, b"").unwrap();
        let link = bindir.join("python");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let out = ro_binds(link.as_os_str(), &[], home.path());
        assert!(
            out.contains(&bindir.canonicalize().unwrap()),
            "literal symlink's dir missing: {out:?}"
        );
        assert!(
            out.contains(&libdir.canonicalize().unwrap()),
            "resolved target's dir missing: {out:?}"
        );
    }

    // --- bare-name program via $PATH ---------------------------------------------------------

    #[test]
    fn resolve_on_path_in_finds_first_executable_match() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("mytool");
        std::fs::write(&exe, b"").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_on_path_in(OsStr::new("mytool"), &path), Some(exe));
    }

    #[test]
    fn resolve_on_path_in_skips_non_executable_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("mytool");
        std::fs::write(&plain, b"").unwrap(); // exists but NOT executable
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_on_path_in(OsStr::new("mytool"), &path), None);
        assert_eq!(resolve_on_path_in(OsStr::new("absent"), &path), None);
    }

    /// A bare-name program installed only under a `$HOME` `PATH` dir (`~/.cargo/bin`, an asdf shim)
    /// is hidden by the home tmpfs, so its resolving directory must be bound. Prepends a home bin
    /// dir to `PATH` (keeping the rest, so concurrent readers still resolve their own binaries) and
    /// installs a uniquely named executable there; a RAII guard restores `PATH` even on panic.
    #[test]
    fn bare_name_program_on_a_home_path_dir_binds_that_dir() {
        struct PathGuard(std::ffi::OsString);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                std::env::set_var("PATH", &self.0);
            }
        }

        let home = tempfile::tempdir().unwrap();
        let bindir = home.path().join(".local/bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let tool = bindir.join("glass-uniq-tool-xyzzy");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cwd = tempfile::tempdir().unwrap();
        let out = {
            let original = std::env::var_os("PATH").unwrap_or_default();
            let mut prepended = bindir.clone().into_os_string();
            prepended.push(":");
            prepended.push(&original);
            std::env::set_var("PATH", &prepended);
            let _guard = PathGuard(original);
            launch_ro_binds(
                OsStr::new("glass-uniq-tool-xyzzy"),
                &[],
                home.path(),
                Some(cwd.path()),
            )
        };
        assert_eq!(out, vec![bindir.canonicalize().unwrap()]);
    }

    // --- relative token resolution against cwd -----------------------------------------------

    /// A relative launch argument (`assets/data.bin`) is resolved against `cwd` and its directory
    /// bound, so a contained launch that names files relative to its working dir reaches them.
    #[test]
    fn relative_arg_is_resolved_against_cwd_and_bound() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let sub = cwd.path().join("assets");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("data.bin"), b"").unwrap();
        let out = launch_ro_binds(
            OsStr::new("python3"),
            &[OsString::from("assets/data.bin")],
            home.path(),
            Some(cwd.path()),
        );
        assert_eq!(out, vec![sub.canonicalize().unwrap()]);
    }

    /// A relative token with NO known `cwd` (`None`) is SKIPPED, not resolved against `/` — so it
    /// never binds a wrong top-level directory. An absolute token in the same call still binds, so
    /// the ONLY bind here is that absolute path's directory.
    #[test]
    fn relative_token_with_no_cwd_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let sub = home.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let abs = sub.join("keep.bin");
        std::fs::write(&abs, b"").unwrap();
        let out = launch_ro_binds(
            OsStr::new("./run.sh"), // relative program → skipped when cwd is None
            &[OsString::from("assets/data.bin"), OsString::from(&abs)],
            home.path(),
            None,
        );
        assert_eq!(out, vec![sub.canonicalize().unwrap()]);
    }

    // --- /tmp guard isolated from home (load-bearing for the /tmp shadowed root) --------------

    /// The `/tmp` guard must hold even when `home` is unrelated to `/tmp`: a real file directly
    /// under `/tmp` is bound file-only (never `/tmp` as a directory), and an arg of exactly `/tmp`
    /// is skipped. `home` is a NONEXISTENT path OUTSIDE `/tmp` (canon falls back to it), so ONLY
    /// the literal `/tmp` entry in `shadowed_roots` protects `/tmp` — remove it and this test
    /// fails (the file's dir `/tmp` would be bound, and `/tmp` itself would bind rather than skip).
    #[test]
    fn tmp_guard_holds_when_home_is_outside_tmp() {
        let home = Path::new("/nonexistent-glass-home-outside-tmp");
        let cwd = tempfile::tempdir().unwrap();
        let file = tempfile::Builder::new().tempfile_in("/tmp").unwrap();

        // A real file directly under /tmp → bind the FILE only, never /tmp as a directory.
        let out = launch_ro_binds(
            OsStr::new("python3"),
            &[OsString::from(file.path())],
            home,
            Some(cwd.path()),
        );
        assert_eq!(out, vec![file.path().canonicalize().unwrap()]);
        assert!(!out.iter().any(|p| *p == Path::new("/tmp")));

        // An arg of exactly /tmp → skipped entirely.
        let out2 = launch_ro_binds(
            OsStr::new("python3"),
            &[OsString::from("/tmp")],
            home,
            Some(cwd.path()),
        );
        assert!(
            out2.is_empty(),
            "an arg of /tmp must be skipped; got {out2:?}"
        );
    }

    // --- secret-isolation invariant ----------------------------------------------------------

    /// The hard invariant over a mixed launch: no produced bind may equal a shadowed root
    /// (`home`, `/tmp`) or be an ancestor of one.
    #[test]
    fn no_bind_equals_a_shadowed_root_or_ancestor() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let in_home = home.path().join("top.py");
        std::fs::write(&in_home, b"").unwrap();
        let tmpfile = tempfile::Builder::new().tempfile_in("/tmp").unwrap();
        std::fs::write(cwd.join("r.sh"), b"").unwrap();
        let out = launch_ro_binds(
            OsStr::new("python3"),
            &[
                OsString::from(&in_home),       // file directly under home
                OsString::from(tmpfile.path()), // file directly under /tmp
                OsString::from("/tmp"),         // a shadowed root itself
                OsString::from(home.path()),    // home itself
                OsString::from("r.sh"),         // relative → cwd/r.sh
            ],
            home.path(),
            Some(cwd.as_path()),
        );
        let home_c = home.path().canonicalize().unwrap();
        let roots = [home_c.as_path(), Path::new("/tmp")];
        for b in &out {
            // `root.starts_with(b)` is true iff `b` equals a root or is an ancestor of one.
            assert!(
                !roots.iter().any(|root| root.starts_with(b)),
                "bind {b:?} equals a shadowed root or an ancestor of one"
            );
        }
        assert!(
            !out.is_empty(),
            "sanity: the launch should still bind something"
        );
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
        let restricted = sandbox_checks(
            false,
            "bwrap",
            Some("uid map: Permission denied".into()),
            true,
        );
        let r = restricted[0].remedy.clone().unwrap();
        assert!(
            r.contains("apparmor_restrict_unprivileged_userns"),
            "got: {r}"
        );

        let generic = sandbox_checks(false, "bwrap", Some("bwrap not found".into()), false);
        let g = generic[0].remedy.clone().unwrap();
        assert!(
            !g.to_lowercase().contains("apparmor"),
            "generic remedy must not claim AppArmor: {g}"
        );
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
        for level in [
            SandboxLevel::Off,
            SandboxLevel::Default,
            SandboxLevel::Strict,
        ] {
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
        assert!(
            run_build(&make_spec(None, SandboxLevel::Off)).is_ok(),
            "no build → Ok"
        );
        assert!(
            run_build(&make_spec(Some("true"), SandboxLevel::Off)).is_ok(),
            "successful build → Ok"
        );
        assert!(
            run_build(&make_spec(Some("false"), SandboxLevel::Off)).is_err(),
            "failing build → Err"
        );
    }

    #[test]
    fn run_build_default_sandbox_runs_and_reports_status() {
        use glass_core::SandboxLevel;
        assert!(
            run_build(&make_spec(None, SandboxLevel::Default)).is_ok(),
            "no build → Ok"
        );
        assert!(
            run_build(&make_spec(Some("true"), SandboxLevel::Default)).is_ok(),
            "successful build → Ok"
        );
        assert!(
            run_build(&make_spec(Some("false"), SandboxLevel::Default)).is_err(),
            "failing build → Err"
        );
    }
}
