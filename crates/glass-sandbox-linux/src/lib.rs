//! Linux process containment for glass via bubblewrap (`bwrap`).
//!
//! `wrap_argv` is pure (builds an argv, touches nothing) so it is unit-tested by
//! asserting the arguments. `availability` runs `bwrap` to prove a user namespace
//! can be created. Callers handle `SandboxLevel::Off` themselves (never wrap).

#![cfg(target_os = "linux")]

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use glass_core::{AppSpec, BoundKind, Check, CheckStatus, GlassError, Result, SandboxLevel};
use glass_exec_unix::{Resolved, resolve_bin, resolve_on_path_in};
use glass_sandbox_unix::{abs_token, canon, dir_of};

/// App-level environment that makes GUI toolkits present frames without X11 MIT-SHM. glass's
/// containment breaks shared-memory rendering on the headless display: `wrap_argv` passes
/// `--unshare-ipc`, which isolates the SysV IPC namespace so MIT-SHM can't attach to the
/// out-of-sandbox X server. That is the operative cause — GTK4's GL renderer, and even the Mesa
/// software (llvmpipe) path it falls back to once `--dev /dev` also withholds `/dev/dri`, both need
/// MIT-SHM to present, so the window stays black. These vars pick a renderer that presents via
/// plain X instead (GTK4's cairo renderer; Qt's non-SHM / software paths); each is a no-op for a
/// toolkit that does not read it.
pub const SOFTWARE_RENDER_ENV: &[(&str, &str)] = &[
    ("GSK_RENDERER", "cairo"),        // GTK4
    ("QT_X11_NO_MITSHM", "1"),        // Qt (X11 widgets)
    ("QT_QUICK_BACKEND", "software"), // Qt Quick / QML
];

/// The [`SOFTWARE_RENDER_ENV`] defaults to inject for a launch: the full set for a contained
/// launch, minus any key the user already set in `spec.env` (an explicit override always wins);
/// empty for `sandbox: off` (the app keeps full GPU/SHM access and may legitimately want GL).
pub fn software_render_env(spec: &AppSpec) -> Vec<(&'static str, &'static str)> {
    if spec.sandbox == SandboxLevel::Off {
        return Vec::new();
    }
    SOFTWARE_RENDER_ENV
        .iter()
        .copied()
        .filter(|(k, _)| !spec.env.iter().any(|(user_key, _)| user_key.as_str() == *k))
        .collect()
}

/// The bubblewrap binary glass invokes: `$GLASS_BWRAP`, else `bwrap` (on `PATH`).
fn bwrap_bin() -> String {
    glass_core::tool_path("GLASS_BWRAP", "bwrap")
}

/// The POSIX shell glass runs the build command with: `$GLASS_SH`, else `sh`.
fn sh_bin() -> String {
    glass_core::tool_path("GLASS_SH", "sh")
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
    launch_ro_binds_in(
        program,
        args,
        home,
        cwd,
        std::env::var_os("PATH").as_deref(),
    )
}

/// [`launch_ro_binds`] against an explicit `$PATH` value — the testable seam (no global env), so
/// a test can exercise the bare-name branch.
fn launch_ro_binds_in(
    program: &OsStr,
    args: &[OsString],
    home: &Path,
    cwd: Option<&Path>,
    path: Option<&OsStr>,
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
    } else if let Some(resolved) = path.and_then(|p| resolve_on_path_in(program, p)) {
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

/// Build the full argv for a contained launch: `bwrap … -- <program> <args…>`.
pub fn wrap_argv(program: &OsStr, args: &[OsString], opts: &WrapOpts) -> Vec<OsString> {
    let mut v: Vec<OsString> = vec![OsString::from(bwrap_bin())];
    for f in [
        "--unshare-user",
        "--unshare-ipc",
        // NOTE: --unshare-pid is intentionally OMITTED: a PID namespace makes the child's
        // std::process::id() return a namespace-relative PID (often 2), which is what it
        // would write into _NET_WM_PID — glass's window discovery then can't match the child,
        // since it holds the host PID.
        //
        // Security note: without a PID namespace the contained process can see host PIDs in
        // /proc and can signal same-UID processes, glass-mcp included. Accepted trade-off —
        // filesystem and network containment are the goals. Passing _NET_WM_PID out-of-band
        // (bwrap --json-status-fd) would restore PID-namespace isolation.
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
        // ~/.ssh etc.) and /tmp. A `--bind <cwd> <cwd>` where cwd equals a shadowed root — or
        // is a parent of one, e.g. cwd="/home" with home="/home/u" — would re-mount the real
        // subtree OVER the tmpfs, re-exposing everything just hidden.
        //
        // `root.starts_with(&cwd_c)` is true in both cases. The common subdir case gives false
        // and is bound rw as usual; `--ro-bind / /` plus the tmpfs still provide the path for
        // the skipped cases, so `--chdir` works.
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
    availability_of(probe(), apparmor_userns_restricted() == Some(true))
}

/// Pure: what a probe means to a caller that needs only go/no-go.
fn availability_of(probe: Probed, apparmor_restricted: bool) -> Availability {
    match probe {
        Ok(_) => Availability::Ok,
        Err(no) => Availability::Unavailable(no.message(apparmor_restricted)),
    }
}

/// What the probe answered: how long the namespace took to create, or why it could not be.
type Probed = std::result::Result<Duration, NoSandbox>;

/// Why the sandbox cannot be used, one variant per remedy — telling causes apart by their message
/// prose is what glass#348 forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
enum NoSandbox {
    /// Nothing to run: no `bwrap` resolved, or the one that did cannot be executed.
    Missing(String),
    /// A bare `bwrap` and no `$PATH` to look it up in, so nothing says whether it is installed.
    NotLookedUp(String),
    /// bwrap ran and exited non-zero, including without saying why.
    Refused(String),
    /// bwrap had not exited when its budget ran out, so it was sent SIGKILL — which one wedged in
    /// the kernel does not take until it surfaces. Only the direct child is signalled; anything it
    /// had already forked outlives it.
    TimedOut(String),
    /// The call could not be completed, so nothing about namespaces was learned. bwrap may well
    /// have run: one that exited leaving something holding its output pipe lands here too.
    Unfinished(String),
}

const INSTALL_BWRAP: &str = "install `bubblewrap`, or point GLASS_BWRAP at a copy this machine can \
     execute; or run with sandbox:\"off\" (GLASS_SANDBOX=off) to launch unconfined";
const NAME_BWRAP: &str = "point GLASS_BWRAP at bubblewrap's absolute path, or start glass with a \
     PATH to search; or run with sandbox:\"off\" (GLASS_SANDBOX=off) to launch unconfined";
const CHECK_THE_MOUNTS: &str = "the probe read-only-binds the whole root, so a mount that has \
     stopped responding (an unreachable network filesystem) can hold it there; check for one, or \
     run with sandbox:\"off\" (GLASS_SANDBOX=off) to launch unconfined";
const NOTHING_LEARNED: &str = "nothing here says the sandbox is broken — glass could not finish \
     asking; try again, or run with sandbox:\"off\" (GLASS_SANDBOX=off) to launch unconfined";

impl NoSandbox {
    fn why(&self) -> &str {
        match self {
            NoSandbox::Missing(why)
            | NoSandbox::NotLookedUp(why)
            | NoSandbox::Refused(why)
            | NoSandbox::TimedOut(why)
            | NoSandbox::Unfinished(why) => why,
        }
    }

    /// The fix, tailored to the cause. A bubblewrap that said nothing gets neither stock remedy:
    /// it is installed, and AppArmor's knob fails `unshare` with EPERM at once.
    fn remedy(&self, apparmor_restricted: bool) -> &'static str {
        match self {
            NoSandbox::Missing(_) => INSTALL_BWRAP,
            NoSandbox::NotLookedUp(_) => NAME_BWRAP,
            NoSandbox::Refused(_) => refusal_remedy(apparmor_restricted),
            NoSandbox::TimedOut(_) => CHECK_THE_MOUNTS,
            NoSandbox::Unfinished(_) => NOTHING_LEARNED,
        }
    }

    /// Cause and fix in one string, for the launch path's single error message.
    fn message(&self, apparmor_restricted: bool) -> String {
        format!("{} — {}", self.why(), self.remedy(apparmor_restricted))
    }
}

/// Resolve `bwrap` and put it to the user-namespace question. The only caller that supplies
/// [`USERNS_PROBE_BUDGET`]; `tests/bwrap_probe_bound.rs` is what holds that wiring in place.
fn probe() -> Probed {
    let bin = bwrap_bin();
    let resolved = resolve_bin(&bin, std::env::var_os("PATH").as_deref());
    userns_probe(&resolved_bwrap(&bin, resolved)?, USERNS_PROBE_BUDGET)
}

/// Pure: what resolving `bwrap` alone decides — the testable seam (no global env). `Ok` carries the
/// file to probe; that it is runnable is all resolution proves.
///
/// `bin` is the configured name, needed for the [`Resolved::Absent`] and
/// [`Resolved::NoSearchPath`] messages because neither variant carries a path.
fn resolved_bwrap(bin: &str, bwrap: Resolved) -> std::result::Result<PathBuf, NoSandbox> {
    match bwrap {
        Resolved::Found(p) => Ok(p),
        Resolved::NotExecutable(p) => Err(NoSandbox::Missing(format!(
            "bubblewrap ({}) is not executable",
            p.display()
        ))),
        Resolved::Absent => Err(NoSandbox::Missing(format!("bubblewrap ({bin}) not found"))),
        // Its own variant, not `Missing`: that one's remedy leads with "install `bubblewrap`",
        // which cannot restore a `PATH` (glass#373).
        Resolved::NoSearchPath => Err(NoSandbox::NotLookedUp(format!(
            "bubblewrap ({bin}) could not be looked up — PATH is unset in glass's environment"
        ))),
    }
}

/// How long `bwrap` gets to answer the user-namespace question.
///
/// The probe does real kernel work: it creates an unprivileged user namespace and `--ro-bind / /`s
/// the root — the same bind `wrap_argv` emits for a real launch, so a mount that hangs the probe
/// would have hung the launch.
const USERNS_PROBE_BUDGET: Duration = Duration::from_secs(10);

/// A shrunken budget is the failure no test can see: every fixture answers in milliseconds, so the
/// suite stays green while no sandboxed launch on a loaded host works at all. The ceiling keeps
/// `tests/bwrap_probe_bound.rs`'s 15s bound discriminating.
const _: () = assert!(USERNS_PROBE_BUDGET.as_secs() >= 5 && USERNS_PROBE_BUDGET.as_secs() <= 15);

/// Run `bwrap` to prove an unprivileged user namespace can actually be created here, returning how
/// long it took to say so.
///
/// A timeout is not stepped over: the launch fails closed rather than hand the app to a bwrap that
/// never answered.
fn userns_probe(bwrap: &Path, budget: Duration) -> Probed {
    let mut cmd = Command::new(bwrap);
    cmd.args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"]);
    let at = bwrap.display();
    let started = Instant::now();
    match glass_core::run_bounded(&mut cmd, budget, "bwrap:userns") {
        Ok(o) if o.status.success() => Ok(started.elapsed()),
        Ok(o) => Err(NoSandbox::Refused(
            match String::from_utf8_lossy(&o.stderr).trim() {
                // Nothing said establishes nothing about namespaces — a signal death (the OOM
                // killer, a segfault) lands here.
                "" => format!("bubblewrap ({at}) failed without saying why ({})", o.status),
                said => format!("bubblewrap ({at}) cannot create a user namespace: {said}"),
            },
        )),
        // The runner's own message carries what bwrap said before it wedged, and whether the kill
        // was collected — a bwrap that SIGKILL could not reach is itself evidence for the mount the
        // remedy asks the reader to go find.
        Err(e) if e.bound() == Some(BoundKind::TimedOut) => {
            Err(NoSandbox::TimedOut(format!("bubblewrap ({at}): {e}")))
        }
        Err(e) => Err(NoSandbox::Unfinished(format!("bubblewrap ({at}): {e}"))),
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

/// Pure: the fix for a bwrap that ran and refused, tailored to whether AppArmor's
/// unprivileged-userns restriction is the likely cause (Ubuntu 23.10+). It never mentions
/// installing bubblewrap: one that answered is installed.
fn refusal_remedy(apparmor_restricted: bool) -> &'static str {
    if apparmor_restricted {
        "this system restricts unprivileged user namespaces via AppArmor \
         (kernel.apparmor_restrict_unprivileged_userns=1), which bubblewrap requires. Allow them \
         with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` (persist via a file \
         in /etc/sysctl.d/), or run with sandbox:\"off\" (GLASS_SANDBOX=off)"
    } else {
        "enable unprivileged user namespaces (e.g. `sysctl kernel.unprivileged_userns_clone=1`), \
         or run with sandbox:\"off\" (GLASS_SANDBOX=off) to launch unconfined"
    }
}

/// Pure: map probed facts to a doctor check, which prints cause and fix in separate columns.
/// `bin` is the configured name, not the resolved path.
fn sandbox_checks(bin: &str, probe: Probed, apparmor_restricted: bool) -> Vec<Check> {
    let name = "sandbox (bubblewrap)";
    let check = match probe {
        // The elapsed time is the only warning of a host near the budget, where doctor and the next
        // launch can disagree about the same machine.
        Ok(took) => Check::new(
            name,
            CheckStatus::Ok,
            format!("{bin} present; user namespaces usable (answered in {took:?})"),
        ),
        Err(no) => Check::new(name, CheckStatus::Fail, no.why())
            .with_remedy(no.remedy(apparmor_restricted)),
    };
    vec![check]
}

/// Gather the live sandbox check.
pub fn checks() -> Vec<Check> {
    let bin = bwrap_bin();
    let apparmor_restricted = apparmor_userns_restricted() == Some(true);
    sandbox_checks(&bin, probe(), apparmor_restricted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::SandboxLevel;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::fs::PermissionsExt;
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
        assert!(
            !out.iter()
                .any(|p| *p == home.path().canonicalize().unwrap())
        );
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

    /// A bare-name program installed only under a `$HOME` `PATH` dir (`~/.cargo/bin`, an asdf shim)
    /// is hidden by the home tmpfs, so its resolving directory must be bound. Passes the `$PATH` to
    /// resolve against explicitly — mutating the process `PATH` would race every other test in this
    /// binary (and is `unsafe` from edition 2024 on, for that reason).
    #[test]
    fn bare_name_program_on_a_home_path_dir_binds_that_dir() {
        let home = tempfile::tempdir().unwrap();
        let bindir = home.path().join(".local/bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let tool = bindir.join("glass-uniq-tool-xyzzy");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cwd = tempfile::tempdir().unwrap();
        let out = launch_ro_binds_in(
            OsStr::new("glass-uniq-tool-xyzzy"),
            &[],
            home.path(),
            Some(cwd.path()),
            Some(bindir.as_os_str()),
        );
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

    /// Every way the sandbox can turn out unusable, for the properties that hold across all of
    /// them. Each carries a cause a real probe would produce.
    fn every_cause() -> Vec<NoSandbox> {
        vec![
            NoSandbox::Missing("bubblewrap (bwrap) not found".into()),
            NoSandbox::Refused(
                "bubblewrap (/usr/bin/bwrap) cannot create a user namespace: \
                                setting up uid map: Permission denied"
                    .into(),
            ),
            NoSandbox::TimedOut(
                "bubblewrap (/usr/bin/bwrap): bwrap:userns: no answer within 10s".into(),
            ),
            NoSandbox::Unfinished(
                "bubblewrap (/usr/bin/bwrap): bwrap:userns: could not check whether the process \
                 had exited"
                    .into(),
            ),
        ]
    }

    #[test]
    fn doctor_reports_ok_and_failure() {
        use glass_core::CheckStatus;
        let ok = sandbox_checks("bwrap", Ok(Duration::from_millis(9)), false);
        assert_eq!(ok[0].status, CheckStatus::Ok);
        for no in every_cause() {
            let bad = sandbox_checks("bwrap", Err(no.clone()), false);
            assert_eq!(bad[0].status, CheckStatus::Fail, "{no:?}");
            assert_eq!(bad[0].detail, no.why(), "{no:?}");
            assert!(bad[0].remedy.is_some(), "{no:?}");
        }
    }

    /// The elapsed time is the only sign of a host near the budget.
    #[test]
    fn doctor_says_how_long_the_probe_took() {
        let ok = sandbox_checks("bwrap", Ok(Duration::from_millis(1250)), false);
        assert!(ok[0].detail.contains("1.25s"), "{:?}", ok[0]);
    }

    #[test]
    fn doctor_remedy_calls_out_apparmor_userns_restriction() {
        // Ubuntu 23.10+ restricts unprivileged userns via AppArmor (bwrap then fails
        // "setting up uid map: Permission denied"). When that's the cause, the remedy must
        // name the exact knob; otherwise it must not falsely claim AppArmor.
        let restricted = sandbox_checks(
            "bwrap",
            Err(NoSandbox::Refused("uid map: Permission denied".into())),
            true,
        );
        let r = restricted[0].remedy.clone().unwrap();
        assert!(
            r.contains("apparmor_restrict_unprivileged_userns"),
            "got: {r}"
        );

        let generic = sandbox_checks(
            "bwrap",
            Err(NoSandbox::Refused("uid map: Permission denied".into())),
            false,
        );
        let g = generic[0].remedy.clone().unwrap();
        assert!(
            !g.to_lowercase().contains("apparmor"),
            "generic remedy must not claim AppArmor: {g}"
        );
    }

    /// A bubblewrap that answered is installed, whatever it answered.
    #[test]
    fn a_refusal_is_never_answered_with_install_it() {
        for apparmor in [false, true] {
            let r = refusal_remedy(apparmor);
            assert!(
                !r.to_lowercase().contains("install"),
                "apparmor={apparmor}: {r}"
            );
        }
    }

    /// A bubblewrap that is not there cannot be refusing anything, so AppArmor's knob is not the
    /// fix — and a restricted host with no bwrap installed is reachable.
    #[test]
    fn an_absent_bwrap_is_never_blamed_on_apparmor() {
        for apparmor in [false, true] {
            let checks = sandbox_checks(
                "bwrap",
                Err(NoSandbox::Missing("bubblewrap (bwrap) not found".into())),
                apparmor,
            );
            let r = checks[0].remedy.clone().unwrap();
            assert!(
                !r.to_lowercase().contains("apparmor") && r.contains("install"),
                "apparmor={apparmor}: {r}"
            );
        }
    }

    /// A bubblewrap that said nothing is installed and may well work, so neither stock remedy fits.
    /// Asserted as the absence of that advice — a remedy that merely appended a clause to either
    /// would pass an inequality.
    #[test]
    fn a_silent_bwrap_is_answered_with_neither_stock_remedy() {
        for apparmor in [false, true] {
            let checks = sandbox_checks(
                "bwrap",
                Err(NoSandbox::TimedOut("bwrap: no answer within 10s".into())),
                apparmor,
            );
            assert_eq!(checks[0].status, glass_core::CheckStatus::Fail);
            let r = checks[0].remedy.clone().unwrap().to_lowercase();
            assert!(!r.contains("install"), "apparmor={apparmor}: {r}");
            assert!(!r.contains("apparmor"), "apparmor={apparmor}: {r}");
            assert!(r.contains("mount"), "must name what to look for: {r}");
        }
    }

    /// Whatever is wrong with the sandbox, a launch can decline it — and the launch path can only
    /// say so if the message it is handed already does.
    #[test]
    fn every_unusable_sandbox_reaches_the_launch_path_with_its_own_fix() {
        for no in every_cause() {
            for apparmor in [false, true] {
                let Availability::Unavailable(msg) = availability_of(Err(no.clone()), apparmor)
                else {
                    panic!("{no:?} must not clear a sandboxed launch");
                };
                assert!(msg.starts_with(no.why()), "the cause is not remade: {msg}");
                assert!(msg.contains(no.remedy(apparmor)), "no fix travelled: {msg}");
                assert!(msg.contains("sandbox:\"off\""), "no way to launch: {msg}");
            }
        }
    }

    #[test]
    fn a_proven_namespace_clears_the_launch_path() {
        assert!(
            matches!(
                availability_of(Ok(Duration::from_millis(9)), false),
                Availability::Ok
            ),
            "a bwrap that created a namespace must not refuse the launch"
        );
    }

    /// glass#374: `runnable` used `is_file()`, so a mode-644 `$GLASS_BWRAP` reached the
    /// user-namespace probe, which forked it and reported "could not run …: Permission denied" —
    /// the launch was refused, no app spawned. What changes: the message names the fix, and glass
    /// stops forking a binary to learn what a `stat` answers.
    #[test]
    fn a_non_executable_bwrap_is_unavailable_and_says_so() {
        let Err(no) = resolved_bwrap(
            "/opt/bin/bwrap",
            Resolved::NotExecutable(PathBuf::from("/opt/bin/bwrap")),
        ) else {
            panic!("a non-executable bwrap must not be probed");
        };
        assert!(
            no.why().contains("/opt/bin/bwrap") && no.why().contains("not executable"),
            "must name the file and say why: {no:?}"
        );
        assert!(
            no.remedy(false).contains("GLASS_BWRAP"),
            "the override is the fix for a bwrap glass cannot run: {no:?}"
        );
    }

    /// Both causes are a bubblewrap glass has nothing to run, so asserting the variant alone cannot
    /// tell them apart.
    #[test]
    fn an_absent_bwrap_is_unavailable_and_says_so() {
        let Err(no) = resolved_bwrap("bwrap", Resolved::Absent) else {
            panic!("an absent bwrap must not be probed");
        };
        assert!(
            no.why().contains("bwrap") && no.why().contains("not found"),
            "actionable message: {no:?}"
        );
    }

    /// glass#373: a stripped environment leaves nothing to look a bare `bwrap` up in, and "not
    /// found" sends the user to install a package they already have.
    #[test]
    fn a_bwrap_that_could_not_be_looked_up_is_not_reported_as_missing() {
        let Err(no) = resolved_bwrap("bwrap", Resolved::NoSearchPath) else {
            panic!("a bwrap that never resolved must not be probed");
        };
        assert!(
            no.why().contains("PATH"),
            "the environment is what is missing: {no:?}"
        );
        let remedy = no.remedy(false);
        assert!(
            remedy.contains("GLASS_BWRAP") && remedy.contains("PATH"),
            "an absolute path or a search list are the ways out: {remedy}"
        );
        // The variant is the remedy here: routed through `Missing`, this reads "install
        // `bubblewrap`" for a machine that may well have it installed.
        assert!(
            !remedy.contains("install"),
            "nothing was searched, so nothing says it is missing: {remedy}"
        );
    }

    /// The arm no other test reaches: making it an error refuses every sandboxed launch on every
    /// host and the suite stays green, since each test that really launches under bwrap is
    /// `#[ignore]`d.
    #[test]
    fn a_runnable_bwrap_clears_the_resolution_stage() {
        assert_eq!(
            resolved_bwrap("bwrap", Resolved::Found(PathBuf::from("/usr/bin/bwrap"))),
            Ok(PathBuf::from("/usr/bin/bwrap")),
            "a runnable bwrap must reach the user-namespace probe, by its resolved path"
        );
    }

    /// The probe's spawn-failure arm, which needs no bwrap on the host: naming the binary is the
    /// only way the user learns which one glass tried. Not [`NoSandbox::Refused`] — nothing here
    /// says anything about namespaces, so the remedy must not send the user to enable them.
    #[test]
    fn a_bwrap_that_cannot_be_spawned_reports_why() {
        let Err(no) = userns_probe(Path::new("/nonexistent/bwrap"), USERNS_PROBE_BUDGET) else {
            panic!("a bwrap that cannot be spawned must not report available");
        };
        assert!(
            matches!(no, NoSandbox::Unfinished(_)),
            "a call that never happened is not a refusal: {no:?}"
        );
        assert!(
            no.why().contains("/nonexistent/bwrap"),
            "must name what it tried to run: {no:?}"
        );
    }

    /// How long the never-answering fixture lives if nothing kills it. Whole seconds because it is
    /// interpolated into `sleep`, and far past any budget these tests pass, so a lost bound fails
    /// them rather than passing.
    const HUNG_FIXTURE_SECS: u64 = 30;

    /// Budget for the test that wants a timeout. Short enough to keep the suite quick; the elapsed
    /// assertion allows ten times it, so a loaded machine does not read a bound that fired as one
    /// that did not.
    const HUNG_PROBE_BUDGET: Duration = Duration::from_millis(300);

    /// A fake `bwrap` running `body`, which refuses any argv but the probe's own.
    ///
    /// Without that guard, dropping the probe's arguments would leave every test here green while
    /// glass asked a real bubblewrap a different question entirely. `$#` as well as `$*`, which
    /// cannot tell six arguments from one containing spaces.
    fn fake_bwrap(dir: &Path, body: &str) -> PathBuf {
        let bin = dir.join("bwrap");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\n[ $# -eq 6 ] || exit 3\n\
                 [ \"$*\" = '--unshare-user --ro-bind / / -- true' ] || exit 3\n{body}"
            ),
        )
        .expect("write the fake bwrap");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        wait_until_executable(&bin);
        bin
    }

    /// `exec` of a just-written file fails ETXTBSY while any process still holds it open for
    /// writing — including a sibling test's child, which inherits every fd across the fork and
    /// drops them only at its own exec. This binary spawns constantly, so without this the probe
    /// tests fail as `Unfinished` about half the time. Spawning is the only way to ask; the argv
    /// guard rejects this call, which is all this needs.
    fn wait_until_executable(bin: &Path) {
        /// ETXTBSY. `ErrorKind::ExecutableFileBusy` would say it, but only on the io_error_more
        /// nightly feature.
        const TEXT_FILE_BUSY: i32 = 26;
        for _ in 0..100 {
            match Command::new(bin).arg("--ready").output() {
                Err(e) if e.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return,
            }
        }
    }

    /// glass#398: the probe ran through `Command::output()`, which waits for the child however long
    /// it takes, so a `bwrap` wedged on a mount under `/` hung `glass_start` and `glass doctor`
    /// with nothing to recover from.
    #[test]
    fn a_bwrap_that_never_answers_is_bounded_and_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `exec`, so the killed pid is the sleeper: a forked one would be orphaned and hold both
        // output pipes for the rest of its 30s, past the end of the test.
        let bin = fake_bwrap(dir.path(), &format!("exec sleep {HUNG_FIXTURE_SECS}\n"));

        let started = Instant::now();
        let Err(no) = userns_probe(&bin, HUNG_PROBE_BUDGET) else {
            panic!("a bwrap that never answered has proven no namespace");
        };
        let elapsed = started.elapsed();
        assert!(
            elapsed < HUNG_PROBE_BUDGET * 10,
            "the budget it was passed is not the one it waited: {elapsed:?}"
        );
        assert!(
            matches!(no, NoSandbox::TimedOut(_)),
            "a bwrap that ran and said nothing has not refused: {no:?}"
        );
        assert!(
            no.why().contains(bin.to_str().unwrap()) && no.why().contains("no answer within"),
            "must name the binary and what it did: {no:?}"
        );
    }

    /// The arm every sandboxed launch on a healthy host takes.
    #[test]
    fn a_bwrap_that_answers_clears_the_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = fake_bwrap(dir.path(), "exit 0\n");
        let took = userns_probe(&bin, USERNS_PROBE_BUDGET)
            .expect("a bwrap that answered the probe's own question must clear it");
        assert!(took < USERNS_PROBE_BUDGET, "{took:?}");
    }

    /// What bubblewrap itself says is the actionable part — "cannot create a user namespace" alone
    /// does not distinguish an AppArmor refusal from a kernel without the feature.
    #[test]
    fn a_bwrap_that_refuses_the_namespace_repeats_what_it_said() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = fake_bwrap(
            dir.path(),
            "echo 'setting up uid map: Permission denied' >&2\nexit 1\n",
        );
        let Err(no) = userns_probe(&bin, USERNS_PROBE_BUDGET) else {
            panic!("a bwrap that refused must not report available");
        };
        assert!(
            no.why().contains("cannot create a user namespace") && no.why().contains("uid map"),
            "must carry bwrap's own words: {no:?}"
        );
    }

    /// A bwrap killed by a signal (the OOM killer, a segfault) exits non-zero saying nothing.
    /// Reporting that as "cannot create a user namespace: " asserts a cause nothing established.
    #[test]
    fn a_bwrap_that_dies_without_a_word_is_not_reported_as_a_namespace_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = fake_bwrap(dir.path(), "exit 1\n");
        let Err(no) = userns_probe(&bin, USERNS_PROBE_BUDGET) else {
            panic!("a bwrap that exited non-zero must not report available");
        };
        assert!(
            !no.why().contains("cannot create a user namespace"),
            "silence is not a refusal to create a namespace: {no:?}"
        );
        assert!(
            no.why().contains("exit status: 1"),
            "the status is all that is left to report: {no:?}"
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

    fn spec_with(sandbox: SandboxLevel, env: Vec<(String, String)>) -> AppSpec {
        AppSpec {
            build: None,
            run: vec!["app".into()],
            cwd: None,
            env,
            window_hint: None,
            timeout_ms: 1000,
            sandbox,
            a11y: false,
        }
    }

    #[test]
    fn software_render_env_is_empty_when_sandbox_off() {
        assert!(software_render_env(&spec_with(SandboxLevel::Off, vec![])).is_empty());
    }

    #[test]
    fn software_render_env_injects_all_defaults_when_contained() {
        assert_eq!(
            software_render_env(&spec_with(SandboxLevel::Default, vec![])),
            SOFTWARE_RENDER_ENV.to_vec()
        );
        // Strict is also a contained level.
        assert_eq!(
            software_render_env(&spec_with(SandboxLevel::Strict, vec![])),
            SOFTWARE_RENDER_ENV.to_vec()
        );
    }

    #[test]
    fn user_env_overrides_the_matching_default() {
        let got = software_render_env(&spec_with(
            SandboxLevel::Default,
            vec![("GSK_RENDERER".into(), "gl".into())],
        ));
        // GSK_RENDERER omitted (user set it); the Qt defaults remain.
        assert_eq!(
            got,
            vec![("QT_X11_NO_MITSHM", "1"), ("QT_QUICK_BACKEND", "software")]
        );
    }

    #[test]
    fn multiple_user_overrides_are_all_excluded() {
        let got = software_render_env(&spec_with(
            SandboxLevel::Default,
            vec![
                ("GSK_RENDERER".into(), "gl".into()),
                ("QT_QUICK_BACKEND".into(), "hardware".into()),
            ],
        ));
        assert_eq!(got, vec![("QT_X11_NO_MITSHM", "1")]);
    }

    #[test]
    fn unrelated_user_env_key_filters_nothing() {
        let got = software_render_env(&spec_with(
            SandboxLevel::Default,
            vec![("FOO".into(), "bar".into())],
        ));
        assert_eq!(got, SOFTWARE_RENDER_ENV.to_vec());
    }
}
