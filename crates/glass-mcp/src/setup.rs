//! `glass-mcp setup`: the guided macOS first-run — install the chosen run integration (an
//! unattended `gui/<uid>` LaunchAgent serving HTTP, or nothing for an attended/stdio
//! client-spawned run), then — for the LaunchAgent — guide the user to enable **GlassMcp.app**
//! in the two TCC panes (Screen Recording, Accessibility) and verify by polling that agent's
//! own `/healthz`, and confirm with `doctor` plus a ready-to-paste MCP-client registration
//! line. The terminal itself never requests a grant: a terminal-attributed TCC grant keys to
//! the terminal, not to GlassMcp.app — so `setup` guides + verifies instead of prompting.
//!
//! This module is split so the parts that don't need macOS are unit-testable on Linux:
//! [`RunMode`], [`registration_line`], [`fill_launch_agent`], [`run_mode_from_flags`],
//! [`is_inside_app_bundle`], [`codesign_report_is_unstable`], [`parse_health_response`], and
//! [`fetch_health`] are pure/cross-platform. The interactive grant flow itself
//! (`#[cfg(target_os = "macos")]` inside [`run`], plumbed through the private `macos_impl`
//! submodule) is macOS-only: permission prompts, `codesign`/`launchctl` shell-outs, real file
//! writes.
//!
//! The thin top-level forwarders to `macos_impl` ([`fetch_health`], `install_launch_agent`,
//! `restart_launch_agent`, `uninstall_launch_agent`) exist because a private `mod` can't be
//! named from a sibling module regardless of its items' visibility, and those items have
//! callers in `onboarding`/`menubar`/`status`.

// `GlassError` itself is only named in the `#[cfg(not(target_os = "macos"))]` arm of `run`
// (and its test) — on a macOS build that arm doesn't exist, so import only `Result` here
// and spell out `glass_core::GlassError` at its one use site to avoid an unused-import
// warning on that platform.
use std::path::Path;

use glass_core::Result;

// `HealthStatus` is named by `parse_health_response` (below) and `fetch_health` (near
// `macos_impl`) unconditionally: both are cross-platform (plain TCP + JSON, no macOS API),
// since `status::run` polls a server's health on every target, not just macOS.
use crate::health::HealthStatus;

/// Extract and validate a [`HealthStatus`] out of a raw `/healthz` HTTP response: split off
/// the body at the blank line, require a `200` status line, then deserialize the body as
/// JSON. `None` for anything short of a genuinely healthy response — text that isn't HTTP at
/// all, a non-200 status, or a body that doesn't deserialize — never a fabricated
/// [`HealthStatus`]. Pure (no IO), so it's unit-tested here directly against literal
/// response text; [`fetch_health`] is the only real caller, feeding it a live socket read.
fn parse_health_response(raw: &str) -> Option<HealthStatus> {
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let status_ok = head.lines().next()?.split_whitespace().nth(1) == Some("200");
    if !status_ok {
        return None;
    }
    serde_json::from_str::<HealthStatus>(body.trim()).ok()
}

/// How the user will run `glass-mcp` after setup completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// Installed as a `gui/<uid>` LaunchAgent serving Streamable HTTP — starts at login,
    /// no client to spawn it. The unattended path.
    Http,
    /// Spawned by the MCP client over stdio, one process per session. The attended path;
    /// nothing is installed.
    Stdio,
}

/// The ready-to-paste MCP-client registration command for the chosen run mode. `app_bin`
/// is the resolved path to the `.app`'s `glass-mcp` binary; `addr` is the HTTP bind
/// address (only used for [`RunMode::Http`]).
pub fn registration_line(mode: RunMode, app_bin: &str, addr: &str) -> String {
    match mode {
        RunMode::Stdio => format!("claude mcp add glass --scope user -- {app_bin}"),
        RunMode::Http => format!("claude mcp add --transport http glass http://{addr}/"),
    }
}

/// The three placeholder literals the shipped plist template
/// (`packaging/macos/tech.fixedwidth.glass.plist`) carries and that [`fill_launch_agent`]
/// substitutes. Named once so [`surviving_placeholders`] can detect a drift between these and
/// the template without re-typing the literals.
const APP_BIN_PLACEHOLDER: &str = "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp";
const ADDR_PLACEHOLDER: &str = "127.0.0.1:7300";
const HOME_PLACEHOLDER: &str = "/Users/YOU";

/// The three runtime values [`fill_launch_agent`] substitutes into the plist template — the
/// app-binary path, the HTTP bind address, and the home directory the two log paths are rooted
/// under. A named holder rather than three adjacent `&str` parameters, so a caller can't
/// transpose two of them into a plausible-but-wrong plist without a field-name mismatch.
#[derive(Clone, Copy, Debug)]
pub struct LaunchAgentFields<'a> {
    /// Absolute path to the `.app`'s `glass-mcp` binary (`ProgramArguments[0]`).
    pub app_bin: &'a str,
    /// The HTTP bind address the LaunchAgent serves on.
    pub addr: &'a str,
    /// The home directory the two log paths (`~/Library/Logs/GlassMcp/*.log`) live under.
    pub home: &'a str,
}

/// Fill the LaunchAgent plist template (`packaging/macos/tech.fixedwidth.glass.plist`):
/// substitute the app-binary path, the HTTP bind address, and the home directory the two
/// log paths are rooted under. `template` is the shipped plist text; returns the
/// ready-to-write plist. Pure string substitution — no IO, so the caller decides where (or
/// whether) to write the result (and can run [`surviving_placeholders`] on it first).
pub fn fill_launch_agent(template: &str, fields: LaunchAgentFields<'_>) -> String {
    template
        .replace(APP_BIN_PLACEHOLDER, fields.app_bin)
        .replace(ADDR_PLACEHOLDER, fields.addr)
        .replace(HOME_PLACEHOLDER, fields.home)
}

/// The template placeholders that survived a [`fill_launch_agent`] — non-empty only when a
/// template literal drifted out of sync with the strings `fill_launch_agent` replaces, so a
/// `.replace` silently no-oped and the filled plist still points at a placeholder. A field
/// whose value equals its own placeholder (the default addr `127.0.0.1:7300`, or an app
/// installed at the default `/Applications/GlassMcp.app`) is legitimate, not a drift, and is
/// excluded — so this never false-flags a default configuration. Pure; the
/// [`RunMode::Http`] install path turns a non-empty result into a fail-closed error rather
/// than writing a broken plist.
pub fn surviving_placeholders(filled: &str, fields: LaunchAgentFields<'_>) -> Vec<&'static str> {
    [
        (APP_BIN_PLACEHOLDER, fields.app_bin),
        (ADDR_PLACEHOLDER, fields.addr),
        (HOME_PLACEHOLDER, fields.home),
    ]
    .into_iter()
    .filter(|&(placeholder, value)| value != placeholder && filled.contains(placeholder))
    .map(|(placeholder, _)| placeholder)
    .collect()
}

/// Decide the run mode from the `--launchagent`/`--no-launchagent` flags alone, without
/// prompting. `None` means neither flag forced a choice — the caller must ask interactively,
/// or fall back to a non-prompting default (`--non-interactive`). Pure; the actual prompt (when
/// both flags are absent and the run is interactive) lives in the macOS-only body of [`run`].
///
/// Precedence: `--launchagent` wins over `--no-launchagent`. Clap's `conflicts_with` makes the
/// `(true, true)` combination unreachable from the CLI, so the `debug_assert!` documents that
/// invariant and trips in debug builds if a future non-clap caller violates it.
pub fn run_mode_from_flags(launchagent: bool, no_launchagent: bool) -> Option<RunMode> {
    debug_assert!(
        !(launchagent && no_launchagent),
        "clap conflicts_with should prevent --launchagent + --no-launchagent"
    );
    if launchagent {
        Some(RunMode::Http)
    } else if no_launchagent {
        Some(RunMode::Stdio)
    } else {
        None
    }
}

/// True if `exe` sits inside a `<name>.app/Contents/MacOS/` bundle — the shape
/// `packaging/macos/build-app.sh` produces. A TCC grant is recorded against the *process's*
/// Designated Requirement (bundle id + signing certificate); running a bare binary outside a
/// bundle means a grant given today has nothing stable to attach to. Advisory only — [`run`]
/// warns, never refuses, so `setup` stays usable from `cargo run`. Pure path-shape matching,
/// no filesystem access.
pub fn is_inside_app_bundle(exe: &Path) -> bool {
    let macos_dir = exe.parent();
    let contents_dir = macos_dir.and_then(Path::parent);
    let bundle_dir = contents_dir.and_then(Path::parent);
    let names = (
        macos_dir.and_then(Path::file_name),
        contents_dir.and_then(Path::file_name),
        bundle_dir.and_then(Path::file_name),
    );
    matches!(
        names,
        (Some(macos), Some(contents), Some(bundle))
            if macos == "MacOS" && contents == "Contents" && bundle.to_string_lossy().ends_with(".app")
    )
}

/// True if a `codesign -dvv` report (stdout and stderr concatenated) shows an unstable
/// signing identity — ad hoc, or plain unsigned — either of which means a grant won't survive
/// a rebuild (see [`is_inside_app_bundle`]'s doc for why that matters). Pure string matching
/// over already-captured text, so it's unit-tested on Linux without shelling out to
/// `codesign`; [`run`] is the only real caller, feeding it a live report.
pub fn codesign_report_is_unstable(report: &str) -> bool {
    let report = report.to_ascii_lowercase();
    report.contains("adhoc") || report.contains("not signed")
}

/// The default install location named in the enable-in-System-Settings guidance when the
/// running binary isn't inside a `*.app` bundle (e.g. a bare `cargo run`), so the instruction
/// still points at a concrete path the user can add with the pane's `＋` button.
#[cfg(any(target_os = "macos", test))]
const DEFAULT_APP_PATH: &str = "/Applications/GlassMcp.app";

/// One line of guided-enable instruction for a TCC pane: enable **GlassMcp.app** (the running
/// server's own responsible process — never this terminal) in Privacy & Security → `label`,
/// naming the `.app` bundle path so the user can add it with the pane's `＋` if it isn't
/// already listed. Deliberately says nothing about a consent dialog / "click Allow": the
/// terminal `setup` no longer requests a grant, so no prompt appears — the grant is granted by
/// toggling GlassMcp.app on in the pane. Pure (no IO); reused by onboarding. Gated to
/// macOS+test so a plain non-test Linux/Windows build doesn't warn it dead.
#[cfg(any(target_os = "macos", test))]
fn enable_instruction(label: &str, app_path: &str) -> String {
    format!(
        "Enable GlassMcp.app in System Settings → Privacy & Security → {label}: \
         toggle it on if listed, otherwise click ＋ and add: {app_path}"
    )
}

/// The `*.app` bundle path to name in [`enable_instruction`]. Walks up from `exe`
/// (`…/GlassMcp.app/Contents/MacOS/glass-mcp`) to the enclosing `*.app` directory when `exe`
/// sits inside a bundle (per [`is_inside_app_bundle`]); otherwise falls back to
/// [`DEFAULT_APP_PATH`], so a bare `cargo run` still yields a concrete path to add. Pure (path
/// shape only, no filesystem access), so it's unit-tested on Linux against fabricated paths.
/// `pub(crate)` so the onboarding module reuses this validated helper (via
/// [`is_inside_app_bundle`]) rather than keeping its own weaker walk-up-to-any-`.app` copy.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn app_bundle_path(exe: &Path) -> String {
    // `is_inside_app_bundle` guarantees the three-parents-up ancestor is the `*.app` dir.
    if is_inside_app_bundle(exe)
        && let Some(bundle) = exe.parent().and_then(Path::parent).and_then(Path::parent)
    {
        return bundle.to_string_lossy().into_owned();
    }
    DEFAULT_APP_PATH.to_string()
}

/// The LaunchAgent's `launchctl` job label — matches the shipped plist's `<key>Label</key>`
/// (`packaging/macos/tech.fixedwidth.glass.plist`) and its filename on disk. Named once so
/// [`launch_agent_target`] and [`launch_agent_plist_path`] can't drift from each other or from
/// the plist itself.
#[cfg(any(target_os = "macos", test))]
const LAUNCH_AGENT_LABEL: &str = "tech.fixedwidth.glass";

/// The `gui/<uid>/<label>` target spec `launchctl bootout`/`bootstrap`/`kickstart` address the
/// LaunchAgent job by, given this process's numeric uid (see `macos_impl::self_uid`). Pure
/// string formatting — no OS call — so it's unit-tested on Linux; [`macos_impl::install_launch_agent`]
/// and [`macos_impl::restart_launch_agent`] are the two real callers, both feeding it a live
/// uid, so the target derivation lives in exactly one place instead of two.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn launch_agent_target(uid: u32) -> String {
    format!("gui/{uid}/{LAUNCH_AGENT_LABEL}")
}

/// The path `~/Library/LaunchAgents/tech.fixedwidth.glass.plist` the LaunchAgent plist is
/// written to and later `bootstrap`ped from, given the resolved `$HOME`. Pure path-joining —
/// no filesystem access — so it's unit-tested on Linux; used by
/// [`macos_impl::install_launch_agent`], which both writes and `bootstrap`s the file.
/// [`macos_impl::restart_launch_agent`] doesn't need it — it restarts the already-loaded job
/// via `launchctl kickstart -k` against [`launch_agent_target`] alone, without touching the
/// plist.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn launch_agent_plist_path(home: &str) -> std::path::PathBuf {
    Path::new(home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

/// The parsed `setup` invocation, forwarded from the `Setup` clap variant (see `cli.rs`). A
/// struct rather than four positional arguments to [`run`] so the three adjacent `bool`s can't
/// be transposed at the call site without a field-name mismatch.
#[derive(Debug, Clone)]
pub struct SetupArgs {
    /// Existing standard account used for private macOS application work.
    pub account: Option<String>,
    /// Fail/assume-a-default instead of prompting (scripting/CI).
    pub non_interactive: bool,
    /// Force the `gui/<uid>` LaunchAgent (unattended `serve --http`) instead of asking.
    pub launchagent: bool,
    /// Force stdio (install nothing) instead of asking.
    pub no_launchagent: bool,
    /// Override the LaunchAgent's HTTP bind address (defaults to `127.0.0.1:7300`).
    pub addr: Option<String>,
}

/// Run `glass-mcp setup`. macOS-only: everywhere else this fails fast with an actionable
/// error rather than pretending to do something.
///
/// The fields mirror the `Setup` clap variant verbatim (see `cli.rs`) so the macOS body can
/// use them without re-threading the signature: `non_interactive` fails instead of
/// prompting (scripting/CI); `launchagent`/`no_launchagent` force the run mode instead of
/// asking; `addr` overrides the LaunchAgent's HTTP bind address.
pub fn run(args: SetupArgs) -> Result<()> {
    let SetupArgs {
        account,
        non_interactive,
        launchagent,
        no_launchagent,
        addr,
    } = args;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (account, non_interactive, launchagent, no_launchagent, addr);
        Err(glass_core::GlassError::Backend(
            "setup is macOS-only".into(),
        ))
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(account) = account.as_deref() {
            let mode = match run_mode_from_flags(launchagent, no_launchagent) {
                Some(mode) => mode,
                None if non_interactive => RunMode::Stdio,
                None => macos_impl::prompt_run_mode(),
            };
            return crate::macos_environment::setup(
                account,
                non_interactive,
                mode == RunMode::Http,
                addr.as_deref(),
            )
            .map_err(|error| glass_core::GlassError::Backend(format!("{error:#}")));
        }
        if glass_macos::private_session::configured()? {
            return crate::macos_environment::resume_setup(
                non_interactive,
                run_mode_from_flags(launchagent, no_launchagent),
                addr.as_deref(),
            )
            .map_err(|error| glass_core::GlassError::Backend(format!("{error:#}")));
        }
        // Step 1: preconditions. Resolve the running binary's own path — used for the
        // signing-identity warning right below, and later as both the LaunchAgent's
        // `ProgramArguments[0]` and the registration line's binary path.
        let exe = std::env::current_exe().map_err(|e| {
            glass_core::GlassError::Backend(format!("resolving the running binary's path: {e}"))
        })?;
        macos_impl::warn_if_signing_identity_unstable(&exe);

        match glass_macos::session_state() {
            glass_macos::SessionState::NoSession => {
                eprintln!(
                    "No account is logged in at the console (or it's sitting at the login \
                     window). glass needs a real GUI login to request permissions and drive \
                     anything — log in at the console, or (once already granted) run \
                     glass-mcp as a `gui/{uid}` LaunchAgent instead (see \
                     docs/how-to/build-from-source.md). Then run `glass-mcp setup` again.",
                    uid = macos_impl::self_uid(),
                );
                return Err(glass_core::GlassError::Backend(
                    "setup needs a logged-in console session".into(),
                ));
            }
            glass_macos::SessionState::Locked => {
                println!(
                    "note: the console session is locked/asleep. That doesn't block installing \
                     the LaunchAgent or enabling GlassMcp.app below, but capture/input won't \
                     work until it's unlocked (`caffeinate -d` keeps the display awake, no sudo \
                     needed)."
                );
            }
            glass_macos::SessionState::Unlocked => {}
        }

        // Step 2: resolve the run mode, the app binary + its `.app` bundle path (for the
        // enable-in-System-Settings guidance), and the HTTP bind address.
        let mode = match run_mode_from_flags(launchagent, no_launchagent) {
            Some(mode) => mode,
            None if non_interactive => RunMode::Stdio, // no prompts allowed; least-invasive default
            None => macos_impl::prompt_run_mode(),
        };
        let app_bin = exe.to_string_lossy().into_owned();
        let app_path = app_bundle_path(&exe);
        let addr = addr.unwrap_or_else(|| macos_impl::DEFAULT_ADDR.to_string());

        // `Some((label, instruction))` per outstanding action once the flow settles; an empty
        // `pending` is the only thing that lets `setup` exit zero.
        let mut pending: Vec<(&'static str, String)> = Vec::new();

        // Step 3: install the LaunchAgent (unattended/HTTP) and, once it's serving, guide the
        // user to enable GlassMcp.app in the two Privacy panes — then verify by polling the
        // *agent's* own `/healthz`. Crucially, this terminal never requests a TCC grant
        // itself: a terminal-attributed grant keys to the terminal, not to GlassMcp.app (the
        // bug this flow fixes). The attended/stdio path installs nothing and only warns.
        match mode {
            RunMode::Http => {
                // A LaunchAgent that loaded but isn't yet serving is an outstanding action,
                // not a success: fold it into the same `pending` / non-zero-exit path a
                // missing grant uses (no-silent-success). Only guide+verify once it's up —
                // polling `/healthz` on a dead agent would just burn the whole timeout.
                if let Some(item) = macos_impl::install_launch_agent(&app_bin, &addr)? {
                    pending.push(item);
                } else {
                    macos_impl::guide_enable_and_verify(&app_path, &addr, &mut pending);
                }
            }
            RunMode::Stdio => println!(
                "\nNot installing the LaunchAgent (attended/stdio). On macOS a stdio server's \
                 TCC grant keys to whichever MCP client spawns it, which is fragile; the HTTP \
                 LaunchAgent (`--launchagent`) is the recommended, grant-bearing setup. If one \
                 is already loaded from a previous run, remove it with: launchctl bootout \
                 gui/{}/tech.fixedwidth.glass",
                macos_impl::self_uid(),
            ),
        }

        // Step 4: confirm via `doctor`, print the copy-paste registration line, and — if a
        // grant is still pending — end on the actionable instruction, exiting non-zero rather
        // than claiming success.
        let backend = crate::default_backend(std::env::var("GLASS_BACKEND").ok().as_deref());
        print!("\n{}", crate::doctor::diagnose(false).render_text(backend));
        println!("\n{}", registration_line(mode, &app_bin, &addr));

        if pending.is_empty() {
            Ok(())
        } else {
            println!();
            for (_, instruction) in &pending {
                println!("{instruction}");
            }
            Err(glass_core::GlassError::PermissionDenied {
                which: pending
                    .iter()
                    .map(|(label, _)| *label)
                    .collect::<Vec<_>>()
                    .join(", "),
                remedy: "grant the permission(s) above, then run `glass-mcp setup` again".into(),
            })
        }
    }
}

/// Raw `GET /healthz` against the running server at `addr` (`host:port`), on a 1s
/// connect+read budget matching [`macos_impl::launch_agent_is_serving`]'s bounded-TCP-connect
/// probe. `None` for anything short of a full, parseable response — an unresolvable `addr`, a
/// refused/timed-out connect, a read timeout, or [`parse_health_response`] rejecting the body
/// — never a fabricated [`HealthStatus`]. Pure `std::net::TcpStream` networking, so — unlike
/// the rest of this module — it isn't macOS-specific: cross-platform so `status::run`
/// (`status.rs`) can poll a running server's health on any OS.
pub(crate) fn fetch_health(addr: &str) -> Option<HealthStatus> {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    // Matches `macos_impl::LIVENESS_CONNECT_TIMEOUT`'s value; a local const rather than a
    // shared one because `macos_impl` (and its constant) are `#[cfg(target_os = "macos")]`
    // and this fn is not.
    const TIMEOUT: Duration = Duration::from_secs(1);

    let sock = addr.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&sock, TIMEOUT).ok()?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    write!(
        stream,
        "GET /healthz HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    parse_health_response(&buf)
}

/// Install (or reload) the LaunchAgent — a thin `pub(crate)` forwarder to
/// [`macos_impl::install_launch_agent`] for `onboarding` (see this module's doc).
#[cfg(target_os = "macos")]
pub(crate) fn install_launch_agent(
    app_bin: &str,
    addr: &str,
) -> Result<Option<(&'static str, String)>> {
    macos_impl::install_launch_agent(app_bin, addr)
}

#[cfg(all(target_os = "macos", feature = "network"))]
pub(crate) fn start_launch_agent(app_bin: &str, addr: &str) -> Result<()> {
    macos_impl::start_launch_agent(app_bin, addr)
}

#[cfg(any(all(target_os = "macos", feature = "network"), test))]
fn start_menu_job(
    loaded: &str,
    app_bin: &str,
    arguments: &[String],
    start: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    menu_job_pid(loaded, app_bin, arguments)?.map_or_else(start, |_| Ok(()))
}

#[cfg(any(all(target_os = "macos", feature = "network"), test))]
fn menu_job_pid(loaded: &str, app_bin: &str, arguments: &[String]) -> anyhow::Result<Option<u32>> {
    check_menu_arguments(app_bin, arguments)?;
    anyhow::ensure!(
        crate::launchd::loaded_job_matches(
            loaded,
            &arguments.iter().map(String::as_str).collect::<Vec<_>>()
        ),
        "the loaded Glass job differs from its saved launch settings; registration retained"
    );
    checked_loaded_agent_pid(loaded)
}

#[cfg(any(all(target_os = "macos", feature = "network"), test))]
fn check_menu_arguments(app_bin: &str, arguments: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        arguments.first().map(String::as_str) == Some(app_bin)
            && arguments.get(1).map(String::as_str) == Some("serve")
            && arguments.iter().skip(2).any(|arg| arg == "--menubar"),
        "the saved Glass job is not this installation's menu-bar server; registration retained"
    );
    Ok(())
}

#[cfg(any(all(target_os = "macos", feature = "network"), all(test, unix)))]
fn publish_launch_agent(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing LaunchAgents directory"))?;
    let temporary = parent.join(format!(".glass-start-{:032x}", rand::random::<u128>()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        match rustix::fs::renameat_with(
            rustix::fs::CWD,
            &temporary,
            rustix::fs::CWD,
            path,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
            Err(error) => Err(error.into()),
        }
    })();
    if temporary.exists() {
        std::fs::remove_file(&temporary)?;
    }
    result
}

#[cfg(target_os = "macos")]
pub(crate) fn verify_private_launch_agent(addr: &str) -> Result<()> {
    macos_impl::verify_private_launch_agent(addr)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn checked_loaded_agent_pid(output: &str) -> anyhow::Result<Option<u32>> {
    use anyhow::Context;

    let mut depth = 0_usize;
    let mut roots = 0;
    let mut pid = None;
    for line in output.lines().map(str::trim) {
        if line == "}" {
            depth = depth
                .checked_sub(1)
                .context("unbalanced launchd job output")?;
        } else if depth == 1
            && line
                .strip_prefix("pid")
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '=']))
        {
            anyhow::ensure!(pid.is_none(), "duplicate launchd worker PID fields");
            let value = line
                .strip_prefix("pid = ")
                .context("invalid launchd worker PID field")?;
            anyhow::ensure!(
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
                "invalid launchd worker PID"
            );
            let value = value.parse::<u32>().context("invalid launchd worker PID")?;
            anyhow::ensure!(value != 0, "invalid launchd worker PID");
            pid = Some(value);
        } else if depth == 0
            && !line.is_empty()
            && !line.ends_with(" = {")
            && !line.ends_with(" => {")
        {
            anyhow::bail!("unexpected text outside launchd job output");
        }
        if line.ends_with(" = {") || line.ends_with(" => {") {
            if depth == 0 {
                roots += 1;
                anyhow::ensure!(roots == 1, "multiple launchd job blocks");
            }
            depth += 1;
        }
    }
    anyhow::ensure!(depth == 0 && roots == 1, "incomplete launchd job output");
    Ok(pid)
}

/// Restart the already-installed LaunchAgent — a thin `pub(crate)` forwarder to
/// [`macos_impl::restart_launch_agent`] (see this module's doc). A fresh process re-reads TCC
/// (the Screen Recording grant is cached per-process at launch), so the menu-bar app's
/// "Restart" item (`crate::menubar`) restarts after a grant changes — independent of
/// `KeepAlive`, which is `false` precisely so the LaunchAgent never does this uninvited.
///
/// Gated to `network` + macOS to match its sole caller: the onboarder re-reads TCC by
/// relaunching a fresh *process* (`open -n`), not by restarting the agent.
#[cfg(all(target_os = "macos", feature = "network"))]
pub(crate) fn restart_launch_agent() -> Result<()> {
    macos_impl::restart_launch_agent()
}

/// Remove the login LaunchAgent so glass stops auto-starting — a thin `pub(crate)` forwarder to
/// [`macos_impl::uninstall_launch_agent`] (see this module's doc).
#[cfg(target_os = "macos")]
pub(crate) fn uninstall_launch_agent() -> Result<()> {
    macos_impl::uninstall_launch_agent()
}

/// The macOS-only glue behind [`run`]'s grant flow: side-effecting (stdin/stdout, shelling
/// out to `codesign`/`launchctl`, real TCC calls), unlike the pure helpers above it in this
/// file. Kept in its own module so its `use` block doesn't have to be repeated per item, and
/// so it's obvious at a glance which parts of `setup.rs` only build on macOS.
#[cfg(target_os = "macos")]
mod macos_impl {
    use std::io::Write as _;
    use std::net::{TcpStream, ToSocketAddrs};
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use glass_core::{GlassError, Result};

    use super::RunMode;

    /// Default LaunchAgent HTTP bind address — matches the shipped plist template and
    /// [`super::registration_line`]'s doctest-style examples.
    pub(super) const DEFAULT_ADDR: &str = "127.0.0.1:7300";

    /// How often [`poll_until`] rechecks a grant, and how long it waits before giving up.
    const POLL_INTERVAL: Duration = Duration::from_secs(2);
    const POLL_TIMEOUT: Duration = Duration::from_secs(60);

    /// Liveness-probe budget for [`install_launch_agent`]: launchd needs a beat to exec the
    /// job, so give it ~5s of ~500ms-apart TCP connects before deciding it isn't serving.
    /// (`super::fetch_health` mirrors this value in its own local `const` — it's
    /// cross-platform and this module is macOS-only, so it can't reuse this constant.)
    const LIVENESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
    const LIVENESS_POLL_INTERVAL: Duration = Duration::from_millis(500);
    const LIVENESS_TIMEOUT: Duration = Duration::from_secs(5);

    /// The embedded LaunchAgent plist template — the same file
    /// [`super::tests::fill_launch_agent_substitutes_the_app_binary`] and friends check
    /// against, so a drift between the two breaks the test rather than shipping silently.
    const PLIST_TEMPLATE: &str =
        include_str!("../../../packaging/macos/tech.fixedwidth.glass.plist");

    /// This process's own numeric uid, via `rustix::process::getuid` (a safe syscall wrapper,
    /// so no `unsafe` FFI, unlike a raw `libc::getuid()` call). Used for `gui/<uid>` LaunchAgent
    /// target specs and hint text. This is *not* an OS-verified console-session owner — just the
    /// running process's uid. `setup` is always run directly by the console user with no `sudo`,
    /// so under that assumption it is the correct `gui/<uid>` target.
    pub(super) fn self_uid() -> u32 {
        rustix::process::getuid().as_raw()
    }

    /// Warn (never fail) if `exe`'s bundle placement or signing identity means a TCC grant
    /// won't survive a rebuild — see [`super::is_inside_app_bundle`] and
    /// [`super::codesign_report_is_unstable`] for the two checks.
    pub(super) fn warn_if_signing_identity_unstable(exe: &Path) {
        if !super::is_inside_app_bundle(exe) {
            println!(
                "note: {} isn't inside a *.app/Contents/MacOS bundle — TCC grants are keyed \
                 to the bundle id + signing identity, so this build won't keep its grant \
                 across a rebuild; see docs/how-to/build-from-source.md.",
                exe.display()
            );
        }
        match Command::new("codesign").arg("-dvv").arg(exe).output() {
            Ok(output) => {
                let mut report = String::from_utf8_lossy(&output.stdout).into_owned();
                report.push_str(&String::from_utf8_lossy(&output.stderr));
                if super::codesign_report_is_unstable(&report) {
                    println!(
                        "note: {} is ad hoc or unsigned — a TCC grant won't stick across \
                         rebuilds; sign it with a stable identity (see \
                         docs/how-to/build-from-source.md#create-a-signing-identity).",
                        exe.display()
                    );
                }
            }
            Err(e) => println!(
                "note: couldn't run `codesign -dvv` on {} ({e}) — skipping the signing-identity check.",
                exe.display()
            ),
        }
    }

    /// Poll `granted` every `interval` until it returns `true` or `timeout` elapses, calling
    /// `on_wait` once per unsuccessful check (progress output only — it doesn't affect the
    /// result). Returns the final read of `granted()`.
    fn poll_until(
        granted: impl Fn() -> bool,
        interval: Duration,
        timeout: Duration,
        mut on_wait: impl FnMut(),
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if granted() {
                return true;
            }
            on_wait();
            std::thread::sleep(interval);
        }
        granted()
    }

    /// Ask which run mode to use when neither `--launchagent` nor `--no-launchagent` forced
    /// one; only reached in an interactive run (`--non-interactive` defaults to `Stdio`
    /// instead of calling this — see `run`). Any I/O failure answers `Stdio`, the
    /// least-invasive option (nothing installed).
    pub(super) fn prompt_run_mode() -> RunMode {
        print!(
            "\nRun glass-mcp unattended as a LaunchAgent (serve --http, starts at login) \
             instead of being spawned by your MCP client over stdio? [y/N] "
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return RunMode::Stdio;
        }
        if line.trim().eq_ignore_ascii_case("y") {
            RunMode::Http
        } else {
            RunMode::Stdio
        }
    }

    /// One macOS TCC pane `setup` guides through: its display label, its System Settings
    /// deep-link URL, and the `/healthz` accessor for the grant it maps to (so the deadline
    /// check can read the matching field back off [`super::fetch_health`]). Named fields
    /// rather than a positional tuple so a consumer can't transpose the label and pane URL
    /// (both `&'static str`) — the same anti-transposition reasoning as [`super::SetupArgs`]
    /// and [`super::LaunchAgentFields`].
    struct GrantPane {
        label: &'static str,
        pane_url: &'static str,
        read: fn(&super::HealthStatus) -> Option<bool>,
    }

    /// The two macOS TCC panes `setup` guides the user through, paired with the `/healthz`
    /// field each maps to. Named once so [`guide_enable_and_verify`] both opens the panes and
    /// reads back the matching grant without re-typing the labels.
    fn grant_panes() -> [GrantPane; 2] {
        [
            GrantPane {
                label: "Screen Recording",
                pane_url: glass_macos::screen_recording_pane_url(),
                read: |h| h.screen_recording,
            },
            GrantPane {
                label: "Accessibility",
                pane_url: glass_macos::accessibility_pane_url(),
                read: |h| h.accessibility,
            },
        ]
    }

    /// Guide the user to enable **GlassMcp.app** — the running LaunchAgent, its own TCC
    /// responsible process — in the two Privacy panes, then verify by polling that agent's
    /// `/healthz`. Called only after [`install_launch_agent`] confirms the agent is serving.
    ///
    /// This deliberately never calls `glass_macos::request_*`: those pop the consent dialog
    /// *for the calling process*, so requesting from the terminal keys the grant to the
    /// terminal, not to GlassMcp.app — the exact mis-attribution this flow exists to avoid.
    /// Instead it opens each pane, prints the [`super::enable_instruction`] line, and waits
    /// for the agent to report both grants held via [`super::fetch_health`] (live-read, never
    /// assumed). Appends a `pending` entry per grant still missing at the [`POLL_TIMEOUT`]
    /// deadline — an unreachable `/healthz` (a `None`) can confirm nothing, so it counts every
    /// unconfirmed grant as pending rather than reporting a false success.
    pub(super) fn guide_enable_and_verify(
        app_path: &str,
        addr: &str,
        pending: &mut Vec<(&'static str, String)>,
    ) {
        let ready = |h: &super::HealthStatus| h.grants_ready();
        // A re-run after the user already enabled both grants: confirm without popping the
        // System Settings panes again.
        if super::fetch_health(addr).as_ref().is_some_and(ready) {
            println!(
                "\n  \u{2713} GlassMcp.app already holds both grants (confirmed via /healthz)."
            );
            return;
        }

        println!("\nEnable GlassMcp.app in System Settings (opening the two panes now):");
        for GrantPane {
            label, pane_url, ..
        } in grant_panes()
        {
            if let Err(e) = glass_macos::open_pane(pane_url) {
                eprintln!(
                    "  note: couldn't open the {label} pane automatically ({e}); open Privacy \
                     & Security > {label} manually."
                );
            }
            println!("  {}", super::enable_instruction(label, app_path));
        }

        let landed = poll_until(
            || super::fetch_health(addr).as_ref().is_some_and(ready),
            POLL_INTERVAL,
            POLL_TIMEOUT,
            || println!("  waiting for GlassMcp.app to be enabled in both panes…"),
        );
        if landed {
            println!("  \u{2713} both grants confirmed via GlassMcp.app's /healthz");
            return;
        }

        // Deadline reached without both grants. Read `/healthz` once more to name the ones
        // still missing; a `None` (unreachable) leaves every grant unconfirmed → all pending.
        let health = super::fetch_health(addr);
        for GrantPane {
            label, read: field, ..
        } in grant_panes()
        {
            if health.as_ref().and_then(field) != Some(true) {
                let instruction = format!(
                    "{} — then run `glass-mcp setup` again.",
                    super::enable_instruction(label, app_path)
                );
                println!("  \u{2717} {label}: not yet enabled for GlassMcp.app.");
                pending.push((label, instruction));
            }
        }
    }

    /// Resolve `$HOME`, or a single actionable error — needed to derive the plist path, the
    /// log directory, and the filled plist's `home` field. [`restart_launch_agent`] doesn't
    /// call this: it restarts via `launchctl kickstart -k`, which addresses the job by
    /// `gui/<uid>/<label>` alone and never touches the plist.
    fn home_dir() -> Result<String> {
        std::env::var("HOME").map_err(|_| {
            GlassError::Backend("HOME is not set; can't resolve ~/Library/LaunchAgents".into())
        })
    }

    fn launch_agent_contents(app_bin: &str, addr: &str) -> Result<(std::path::PathBuf, String)> {
        let home = home_dir()?;
        let plist_path = super::launch_agent_plist_path(&home);
        // `plist_path`'s parent is always `~/Library/LaunchAgents` (see
        // `super::launch_agent_plist_path`) — derive it rather than re-joining the path
        // ourselves, so the two can't drift.
        let launch_agents_dir = plist_path.parent().expect("plist_path has a parent dir");
        let logs_dir = Path::new(&home).join("Library/Logs/GlassMcp");
        std::fs::create_dir_all(launch_agents_dir)?;
        std::fs::create_dir_all(&logs_dir)?;

        let fields = super::LaunchAgentFields {
            app_bin,
            addr,
            home: &home,
        };
        let filled = super::fill_launch_agent(PLIST_TEMPLATE, fields);
        // Fail closed on a template drift: if a placeholder survived substitution, the plist is
        // broken (points at `/Users/YOU` etc.) — error rather than write it.
        let survivors = super::surviving_placeholders(&filled, fields);
        if !survivors.is_empty() {
            return Err(GlassError::Backend(format!(
                "LaunchAgent plist still contains template placeholder(s) {survivors:?} after \
                 substitution — packaging/macos/tech.fixedwidth.glass.plist and setup.rs have \
                 drifted; refusing to write a broken plist."
            )));
        }
        Ok((plist_path, filled))
    }

    fn write_launch_agent(app_bin: &str, addr: &str) -> Result<std::path::PathBuf> {
        let (plist_path, contents) = launch_agent_contents(app_bin, addr)?;
        std::fs::write(&plist_path, contents)?;
        Ok(plist_path)
    }

    /// Replace the integration and confirm that its server accepts connections.
    pub(super) fn install_launch_agent(
        app_bin: &str,
        addr: &str,
    ) -> Result<Option<(&'static str, String)>> {
        let plist_path = write_launch_agent(app_bin, addr)?;

        let uid = self_uid();
        let target = super::launch_agent_target(uid);
        crate::launchd::stop(&target, uid)
            .map_err(|error| GlassError::Backend(format!("{error:#}")))?;

        let status = Command::new("launchctl")
            .arg("bootstrap")
            .arg(format!("gui/{uid}"))
            .arg(&plist_path)
            .status()
            .map_err(|e| GlassError::Backend(format!("launchctl bootstrap: {e}")))?;
        if !status.success() {
            return Err(GlassError::Backend(format!(
                "launchctl bootstrap exited {status} — try `launchctl bootout {target}` then \
                 re-run, or pass --no-launchagent to skip installing it."
            )));
        }

        // `bootstrap` succeeding only means launchd accepted the job spec, not that the process
        // came up serving: e.g. a port clash on `--addr` fails the job (once — `KeepAlive` is
        // `false`, so launchd won't keep retrying it) yet bootstrap still returns success.
        // Confirm real liveness before claiming success.
        if launch_agent_is_serving(addr) {
            println!(
                "\n  \u{2713} installed + started {target} ({})",
                plist_path.display()
            );
            Ok(None)
        } else {
            let instruction = format!(
                "LaunchAgent {target} loaded but isn't accepting connections on {addr} yet — \
                 check ~/Library/Logs/GlassMcp/stderr.log (e.g. a port clash on --addr), \
                 resolve it, then run `glass-mcp setup` again."
            );
            println!("\n  \u{2717} {instruction}");
            Ok(Some(("LaunchAgent", instruction)))
        }
    }

    #[cfg(any(feature = "network", test))]
    pub(super) fn read_menu_arguments(plist_path: &Path, app_bin: &str) -> Result<Vec<String>> {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let file = std::fs::File::from(
            rustix::fs::open(
                plist_path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || ![0, self_uid()].contains(&metadata.uid())
            || metadata.mode() & 0o022 != 0
            || metadata.len() > 65536
        {
            return Err(GlassError::Backend(
                "saved Glass job is not a protected regular file; registration retained".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes)?;
        if bytes.len() > 65536 {
            return Err(GlassError::Backend(
                "saved Glass job exceeds its size bound".into(),
            ));
        }
        let saved: plist::Dictionary = plist::from_bytes(&bytes)
            .map_err(|error| GlassError::Backend(format!("read saved Glass job: {error}")))?;
        if saved.get("Label").and_then(plist::Value::as_string) != Some("tech.fixedwidth.glass") {
            return Err(GlassError::Backend("saved Glass job label changed".into()));
        }
        if saved
            .get("Program")
            .is_some_and(|value| value.as_string() != Some(app_bin))
        {
            return Err(GlassError::Backend(
                "saved Glass program differs from this installation; registration retained".into(),
            ));
        }
        let arguments = saved
            .get("ProgramArguments")
            .and_then(plist::Value::as_array)
            .and_then(|arguments| {
                arguments
                    .iter()
                    .map(|value| value.as_string().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| GlassError::Backend("saved Glass job arguments are invalid".into()))?;
        super::check_menu_arguments(app_bin, &arguments)
            .map_err(|error| GlassError::Backend(format!("{error:#}")))?;
        Ok(arguments)
    }

    #[cfg(feature = "network")]
    pub(super) fn start_launch_agent(app_bin: &str, addr: &str) -> Result<()> {
        let target = super::launch_agent_target(self_uid());
        let inspect = || {
            crate::launchd::inspect(&target)
                .map_err(|error| GlassError::Backend(format!("{error:#}")))
        };
        let loaded = inspect()?;
        let plist_path = super::launch_agent_plist_path(&home_dir()?);
        if loaded.is_none() && !plist_path.try_exists()? {
            let (path, contents) = launch_agent_contents(app_bin, addr)?;
            super::publish_launch_agent(&path, contents.as_bytes())?;
        }
        let arguments = read_menu_arguments(&plist_path, app_bin)?;
        let request = || -> anyhow::Result<()> {
            super::check_menu_arguments(app_bin, &arguments)?;
            let mut command = Command::new("/bin/launchctl");
            if loaded.is_some() {
                command.args(["kickstart", &target]);
            } else {
                command
                    .arg("bootstrap")
                    .arg(format!("gui/{}", self_uid()))
                    .arg(&plist_path);
            }
            let result = glass_core::run_bounded(
                command.env("LC_ALL", "C"),
                LIVENESS_TIMEOUT,
                "starting the Glass menu-bar server",
            )?;
            anyhow::ensure!(
                result.status.success(),
                "launchd did not start the Glass menu: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            );
            Ok(())
        };
        let requested = match &loaded {
            Some(loaded) => super::start_menu_job(loaded, app_bin, &arguments, request),
            None => request(),
        };
        let deadline = Instant::now() + LIVENESS_TIMEOUT;
        loop {
            if let Some(loaded) = inspect()?
                && super::menu_job_pid(&loaded, app_bin, &arguments)
                    .map_err(|error| GlassError::Backend(format!("{error:#}")))?
                    .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(GlassError::Backend(match requested {
                    Err(error) => format!("{error:#}"),
                    Ok(()) => {
                        "the Glass menu-bar server did not start; registration retained".into()
                    }
                }));
            }
            std::thread::sleep(LIVENESS_POLL_INTERVAL);
        }
    }

    pub(super) fn verify_private_launch_agent(addr: &str) -> Result<()> {
        let target = super::launch_agent_target(self_uid());
        let ready = poll_until(
            || {
                let Ok(job) = Command::new("/bin/launchctl")
                    .args(["print", &target])
                    .output()
                else {
                    return false;
                };
                if !job.status.success() {
                    return false;
                }
                let Ok(Some(pid)) =
                    super::checked_loaded_agent_pid(&String::from_utf8_lossy(&job.stdout))
                else {
                    return false;
                };
                super::fetch_health(addr).is_some_and(|health| health.private_account_ready(pid))
            },
            LIVENESS_POLL_INTERVAL,
            LIVENESS_TIMEOUT,
            || {},
        );
        if ready {
            Ok(())
        } else {
            Err(GlassError::Backend(format!(
                "the managed Glass HTTP server at {addr} did not confirm account routing; check for a port conflict and rerun setup --account ACCOUNT --launchagent"
            )))
        }
    }

    /// Restart the already-installed LaunchAgent so a fresh process re-reads TCC (the Screen
    /// Recording grant is cached per-process at launch) — independent of `KeepAlive`, which is
    /// `false` precisely so the LaunchAgent itself never respawns on its own after a user Quit.
    ///
    /// Uses `launchctl kickstart -k gui/<uid>/<label>`, *not* `bootout`+`bootstrap`: the
    /// menu-bar app's "Restart" item runs this from *inside* the very LaunchAgent job it's
    /// restarting, and `bootout` would SIGTERM that process directly — killing the caller
    /// before `bootstrap` ever runs, with `KeepAlive=false` meaning launchd then never brings
    /// it back. `kickstart -k` instead asks launchd itself (a separate, always-running
    /// supervisor) to kill and restart the job in place, so it works even when the caller is
    /// the job being restarted, and doesn't depend on `KeepAlive` at all. `kickstart -k`
    /// requires the target job to already be loaded — true of its caller: the menu-bar app
    /// *is* the running LaunchAgent job.
    ///
    /// Errors surface (`kickstart` failing to spawn, or exiting non-zero) rather than being
    /// silently swallowed.
    #[cfg(feature = "network")]
    pub(super) fn restart_launch_agent() -> Result<()> {
        let target = super::launch_agent_target(self_uid());

        let status = Command::new("launchctl")
            .arg("kickstart")
            .arg("-k")
            .arg(&target)
            .status()
            .map_err(|e| GlassError::Backend(format!("launchctl kickstart: {e}")))?;
        if !status.success() {
            return Err(GlassError::Backend(format!(
                "launchctl kickstart -k exited {status} while restarting {target}"
            )));
        }
        Ok(())
    }

    /// Remove the login LaunchAgent so glass stops auto-starting: delete the plist first (so a
    /// later login can't `RunAtLoad` it even if `bootout` races), then bootout the running job.
    /// `bootout` on an already-gone job is fine (its error is ignored, the same way
    /// [`install_launch_agent`]'s idempotent-reload unload does); a plist-removal error is
    /// surfaced — the caller shouldn't report success while the plist (and thus the
    /// starts-at-login behavior) is still on disk.
    pub(super) fn uninstall_launch_agent() -> Result<()> {
        let home = home_dir()?;
        let plist_path = super::launch_agent_plist_path(&home);
        if plist_path.exists() {
            std::fs::remove_file(&plist_path).map_err(|e| {
                GlassError::Backend(format!("remove {}: {e}", plist_path.display()))
            })?;
        }
        let target = super::launch_agent_target(self_uid());
        let _ = Command::new("launchctl")
            .arg("bootout")
            .arg(target)
            .status();
        Ok(())
    }

    /// Bounded liveness probe for a just-bootstrapped agent: TCP-connect to `addr` on a
    /// [`LIVENESS_POLL_INTERVAL`] cadence up to [`LIVENESS_TIMEOUT`], returning `true` on the
    /// first successful connect. An `addr` that doesn't resolve to a socket address yields
    /// `false` — we can't verify it, so it's treated as not-yet-serving, never a silent
    /// success.
    fn launch_agent_is_serving(addr: &str) -> bool {
        let Some(sock) = addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
        else {
            return false;
        };
        poll_until(
            || TcpStream::connect_timeout(&sock, LIVENESS_CONNECT_TIMEOUT).is_ok(),
            LIVENESS_POLL_INTERVAL,
            LIVENESS_TIMEOUT,
            || {},
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_health_response -----------------------------------------------------------

    #[test]
    fn parses_healthz_body_out_of_an_http_response() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
                   {\"ok\":true,\"screen_recording\":true,\"accessibility\":false}";
        let h = super::parse_health_response(raw).unwrap();
        assert_eq!(h.screen_recording, Some(true));
        assert_eq!(h.accessibility, Some(false));
        assert!(!h.grants_ready());
    }

    #[test]
    fn parse_health_response_rejects_garbage() {
        assert!(super::parse_health_response("not http").is_none());
        assert!(super::parse_health_response("HTTP/1.1 500 X\r\n\r\nnope").is_none());
    }

    // --- enable_instruction --------------------------------------------------------------

    #[test]
    fn enable_instruction_names_the_app_and_path() {
        let s = super::enable_instruction("Screen Recording", "/Applications/GlassMcp.app");
        assert!(s.contains("GlassMcp.app"));
        assert!(s.contains("Screen Recording"));
        assert!(s.contains("/Applications/GlassMcp.app"));
        assert!(!s.to_lowercase().contains("click allow")); // no phantom prompt
    }

    // --- app_bundle_path -----------------------------------------------------------------

    #[test]
    fn app_bundle_path_walks_up_to_the_dot_app_dir() {
        assert_eq!(
            app_bundle_path(Path::new(
                "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp"
            )),
            "/Applications/GlassMcp.app"
        );
        // A non-default install location is honored, not overwritten with the fallback.
        assert_eq!(
            app_bundle_path(Path::new("/opt/GlassMcp.app/Contents/MacOS/glass-mcp")),
            "/opt/GlassMcp.app"
        );
    }

    #[test]
    fn app_bundle_path_falls_back_for_a_bare_binary() {
        // A bare `cargo run` output isn't inside a bundle → name the default install path so
        // the instruction still points somewhere concrete.
        assert_eq!(
            app_bundle_path(Path::new("/home/u/glass/target/release/glass-mcp")),
            DEFAULT_APP_PATH
        );
    }

    // --- launch_agent_target / launch_agent_plist_path (shared by install/restart) --------

    #[test]
    fn launch_agent_target_is_the_gui_uid_spec() {
        assert_eq!(launch_agent_target(501), "gui/501/tech.fixedwidth.glass");
    }

    #[test]
    fn launch_agent_plist_path_is_rooted_under_home_library_launchagents() {
        assert_eq!(
            launch_agent_plist_path("/Users/alice"),
            Path::new("/Users/alice/Library/LaunchAgents/tech.fixedwidth.glass.plist")
        );
    }

    // --- registration_line ------------------------------------------------------------

    #[test]
    fn stdio_registration_line_names_the_binary() {
        let line = registration_line(
            RunMode::Stdio,
            "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp",
            "127.0.0.1:7300",
        );
        assert!(line.contains("/Applications/GlassMcp.app/Contents/MacOS/glass-mcp"));
        assert!(line.contains("mcp add glass"));
    }

    #[test]
    fn stdio_registration_line_does_not_use_http_transport() {
        let line = registration_line(RunMode::Stdio, "/bin/glass-mcp", "127.0.0.1:7300");
        assert!(!line.contains("--transport http"));
    }

    #[test]
    fn http_registration_line_names_the_addr() {
        let line = registration_line(RunMode::Http, "/bin/glass-mcp", "127.0.0.1:7300");
        assert!(line.contains("127.0.0.1:7300"));
        assert!(line.contains("--transport http"));
    }

    // --- fill_launch_agent -------------------------------------------------------------

    /// The real shipped template — kept in sync with `packaging/macos/tech.fixedwidth.glass.plist`
    /// by inclusion, so a drift in either place breaks this test rather than shipping silently.
    const TEMPLATE: &str = include_str!("../../../packaging/macos/tech.fixedwidth.glass.plist");

    #[test]
    fn fill_launch_agent_substitutes_the_app_binary() {
        let filled = fill_launch_agent(
            TEMPLATE,
            LaunchAgentFields {
                app_bin: "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp",
                addr: "127.0.0.1:7300",
                home: "/Users/alice",
            },
        );
        assert!(filled.contains("/Applications/GlassMcp.app/Contents/MacOS/glass-mcp"));
    }

    #[test]
    fn fill_launch_agent_substitutes_a_custom_app_binary() {
        let filled = fill_launch_agent(
            TEMPLATE,
            LaunchAgentFields {
                app_bin: "/opt/glass/glass-mcp",
                addr: "127.0.0.1:7300",
                home: "/Users/alice",
            },
        );
        assert!(filled.contains("/opt/glass/glass-mcp"));
        assert!(!filled.contains("/Applications/GlassMcp.app"));
    }

    #[test]
    fn fill_launch_agent_substitutes_the_addr() {
        let filled = fill_launch_agent(
            TEMPLATE,
            LaunchAgentFields {
                app_bin: "/opt/glass/glass-mcp",
                addr: "0.0.0.0:9999",
                home: "/Users/alice",
            },
        );
        assert!(filled.contains("0.0.0.0:9999"));
        assert!(!filled.contains("127.0.0.1:7300"));
    }

    #[test]
    fn fill_launch_agent_substitutes_the_home_in_both_log_paths() {
        let filled = fill_launch_agent(
            TEMPLATE,
            LaunchAgentFields {
                app_bin: "/opt/glass/glass-mcp",
                addr: "127.0.0.1:7300",
                home: "/Users/alice",
            },
        );
        assert!(filled.contains("/Users/alice/Library/Logs/GlassMcp/stdout.log"));
        assert!(filled.contains("/Users/alice/Library/Logs/GlassMcp/stderr.log"));
        assert!(!filled.contains("/Users/YOU"));
    }

    #[test]
    fn fill_launch_agent_pins_a_writable_working_directory() {
        // launchd's default cwd `/` is read-only and breaks glass_baseline_save; the LaunchAgent
        // must pin a writable WorkingDirectory (the resolved home).
        let filled = fill_launch_agent(
            TEMPLATE,
            LaunchAgentFields {
                app_bin: "/opt/glass/glass-mcp",
                addr: "127.0.0.1:7300",
                home: "/Users/alice",
            },
        )
        .replace("\r\n", "\n");
        assert!(filled.contains("<key>WorkingDirectory</key>\n\t<string>/Users/alice</string>"));
    }

    // --- plist template shape (menu-bar launch, KeepAlive) --------------------------------

    #[test]
    fn plist_template_launches_menubar_and_does_not_keepalive() {
        // The LaunchAgent must launch the visible menu-bar mode, and must not fight a user's
        // Quit by relaunching (`KeepAlive=false`; `RunAtLoad` alone starts it at login).
        // Normalize line endings first: on Windows the `.plist` can be checked out with CRLF,
        // and the key/value assertions below span a line boundary (`</key>\n\t<…/>`).
        let t = TEMPLATE.replace("\r\n", "\n");
        assert!(t.contains("<string>--menubar</string>"));
        assert!(t.contains("<key>KeepAlive</key>\n\t<false/>"));
        assert!(!t.contains("<key>KeepAlive</key>\n\t<true/>"));
    }

    // --- surviving_placeholders (template-drift guard) -----------------------------------

    #[test]
    fn a_fully_substituted_plist_has_no_surviving_placeholders() {
        let fields = LaunchAgentFields {
            app_bin: "/opt/glass/glass-mcp",
            addr: "0.0.0.0:9999",
            home: "/Users/alice",
        };
        let filled = fill_launch_agent(TEMPLATE, fields);
        assert!(surviving_placeholders(&filled, fields).is_empty());
    }

    #[test]
    fn default_addr_and_app_path_are_not_reported_as_drift() {
        // The default addr equals its placeholder, and a default-location install's app path
        // contains the app-path placeholder — both legitimate, neither a drift.
        let fields = LaunchAgentFields {
            app_bin: "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp",
            addr: "127.0.0.1:7300",
            home: "/Users/alice",
        };
        let filled = fill_launch_agent(TEMPLATE, fields);
        assert!(surviving_placeholders(&filled, fields).is_empty());
    }

    #[test]
    fn an_unsubstituted_placeholder_is_reported() {
        // A drifted template whose home placeholder `fill_launch_agent` no longer matches: the
        // filled text still carries `/Users/YOU` though a real home (`/Users/alice`) was asked
        // for, so the guard must flag it.
        let fields = LaunchAgentFields {
            app_bin: "/opt/glass/glass-mcp",
            addr: "0.0.0.0:9999",
            home: "/Users/alice",
        };
        let drifted = "<string>/Users/YOU/Library/Logs/GlassMcp/stdout.log</string>";
        assert_eq!(surviving_placeholders(drifted, fields), vec!["/Users/YOU"]);
    }

    // --- run_mode_from_flags -------------------------------------------------------------

    #[test]
    fn launchagent_flag_forces_http() {
        assert_eq!(run_mode_from_flags(true, false), Some(RunMode::Http));
    }

    #[test]
    fn no_launchagent_flag_forces_stdio() {
        assert_eq!(run_mode_from_flags(false, true), Some(RunMode::Stdio));
    }

    #[test]
    fn neither_flag_leaves_it_unresolved() {
        assert_eq!(run_mode_from_flags(false, false), None);
    }

    // --- is_inside_app_bundle -------------------------------------------------------------

    #[test]
    fn recognizes_a_real_app_bundle_layout() {
        assert!(is_inside_app_bundle(Path::new(
            "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp"
        )));
    }

    #[test]
    fn recognizes_a_non_default_install_location() {
        assert!(is_inside_app_bundle(Path::new(
            "/opt/GlassMcp.app/Contents/MacOS/glass-mcp"
        )));
    }

    #[test]
    fn rejects_a_bare_cargo_build_output_path() {
        assert!(!is_inside_app_bundle(Path::new(
            "/home/u/glass/target/release/glass-mcp"
        )));
    }

    #[test]
    fn rejects_a_relative_or_too_shallow_path() {
        assert!(!is_inside_app_bundle(Path::new("glass-mcp")));
        assert!(!is_inside_app_bundle(Path::new("MacOS/glass-mcp")));
    }

    #[test]
    fn rejects_wrong_cased_bundle_directories() {
        // "Contents"/"MacOS" is exact Apple bundle casing; a near-miss shouldn't pass.
        assert!(!is_inside_app_bundle(Path::new(
            "/Applications/GlassMcp.app/contents/macos/glass-mcp"
        )));
    }

    // --- codesign_report_is_unstable ------------------------------------------------------

    #[test]
    fn adhoc_signature_is_unstable() {
        let report = "Executable=/Applications/GlassMcp.app/Contents/MacOS/glass-mcp\n\
                       Identifier=tech.fixedwidth.glass\n\
                       Format=Mach-O thin (arm64)\n\
                       Signature=adhoc\n";
        assert!(codesign_report_is_unstable(report));
    }

    #[test]
    fn unsigned_binary_is_unstable() {
        let report = "glass-mcp: code object is not signed at all\n";
        assert!(codesign_report_is_unstable(report));
    }

    #[test]
    fn a_stable_identity_is_not_flagged() {
        // A real (non-adhoc) code-signing identity's report never contains "adhoc" or "not
        // signed" — this is the shape `codesign -dvv` produces for a self-signed cert made
        // via Keychain Access (see docs/how-to/build-from-source.md#create-a-signing-identity).
        let report = "Executable=/Applications/GlassMcp.app/Contents/MacOS/glass-mcp\n\
                       Identifier=tech.fixedwidth.glass\n\
                       Format=Mach-O thin (arm64)\n\
                       CodeDirectory v=20400 size=411 flags=0x0(none) hashes=8+3 location=embedded\n\
                       Signature=DER encoded\n\
                       Authority=glass-mcp signing\n\
                       TeamIdentifier=not set\n";
        assert!(!codesign_report_is_unstable(report));
    }

    #[test]
    fn checked_loaded_agent_pid_distinguishes_idle_from_running() {
        assert_eq!(
            checked_loaded_agent_pid("job = {\n state = not running\n child = {\n pid = 9\n }\n}")
                .unwrap(),
            None
        );
        assert_eq!(
            checked_loaded_agent_pid("job = {\n pid = 42\n}").unwrap(),
            Some(42)
        );
    }

    #[test]
    fn checked_loaded_agent_pid_refuses_ambiguous_or_incomplete_output() {
        for invalid in [
            "",
            "job = {\n pid = 0\n}",
            "job = {\n pid = invalid\n}",
            "job = {\n pid=42\n}",
            "job = {\n pid =42\n}",
            "job = {\n pid = 42\n pid = 43\n}",
            "job = {\n pid = 42\n",
            "job = {\n child = {\n}\n",
            "job = {\n}\nother = {\n}",
            "pid = 42\n}",
        ] {
            assert!(checked_loaded_agent_pid(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn managed_pid_is_read_only_from_the_loaded_job() {
        assert_eq!(
            checked_loaded_agent_pid(
                "gui/501/job = {\n state = running\n child = {\n pid = 9\n }\n pid = 42\n}\n"
            )
            .unwrap(),
            Some(42)
        );
        assert_eq!(
            checked_loaded_agent_pid(
                "job = {\n environment = {\n \"reqs\" => {\n pid = 9\n }\n }\n pid = 42\n}"
            )
            .unwrap(),
            Some(42)
        );
        for invalid in [
            "job = {\n pid = 0\n}",
            "job = {\n pid = invalid\n}",
            "job = {\n pid = 42\n pid = 43\n}",
            "job = {\n pid = 42\n",
            "pid = 42\n}",
        ] {
            assert!(checked_loaded_agent_pid(invalid).is_err(), "{invalid}");
        }
    }

    fn menu_job(pid: &str, arguments: &[String]) -> String {
        format!(
            "gui/501/tech.fixedwidth.glass = {{\n program = {}\n arguments = {{\n{}\n }}\n{pid}\n}}\n",
            arguments[0],
            arguments
                .iter()
                .map(|arg| format!("  {arg}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    fn menu_arguments() -> Vec<String> {
        [
            "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp",
            "serve",
            "--http",
            "--menubar",
            "--addr",
            "127.0.0.1:7333",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn opening_a_running_menu_preserves_its_process_and_custom_settings() {
        let arguments = menu_arguments();
        start_menu_job(
            &menu_job(" pid = 42", &arguments),
            &arguments[0],
            &arguments,
            || panic!("opening a running menu must not dispatch a start or restart"),
        )
        .unwrap();
    }

    #[test]
    fn opening_an_idle_menu_requests_one_start() {
        let arguments = menu_arguments();
        let mut starts = 0;
        start_menu_job(&menu_job("", &arguments), &arguments[0], &arguments, || {
            starts += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(starts, 1);
    }

    #[test]
    fn an_invalid_menu_binding_or_pid_never_dispatches_start() {
        let arguments = menu_arguments();
        let mut wrong_role = arguments.clone();
        wrong_role[1] = "macos-worker".into();
        let mut wrong_settings = arguments.clone();
        wrong_settings[5] = "127.0.0.1:7444".into();
        for loaded in [
            menu_job("", &wrong_role),
            menu_job("", &wrong_settings),
            menu_job(" pid = 0", &arguments),
            menu_job(" pid = unknown", &arguments),
            menu_job(" pid = 42\n pid = 43", &arguments),
            menu_job("", &arguments)
                .trim_end_matches(['}', '\n'])
                .into(),
        ] {
            assert!(
                start_menu_job(&loaded, &arguments[0], &arguments, || {
                    panic!("invalid binding must not dispatch a start")
                })
                .is_err(),
                "{loaded}"
            );
        }
        assert!(
            start_menu_job(
                &menu_job("", &arguments),
                "/Applications/Other.app/Contents/MacOS/glass-mcp",
                &arguments,
                || { panic!("another installation must not dispatch a start") }
            )
            .is_err()
        );
        assert!(
            start_menu_job(
                &menu_job("", &wrong_role),
                &arguments[0],
                &wrong_role,
                || { panic!("a worker role must not be started as a menu") }
            )
            .is_err()
        );
    }

    #[test]
    fn a_menu_start_error_is_returned_without_replaying_the_request() {
        let arguments = menu_arguments();
        let mut starts = 0;
        let result = start_menu_job(&menu_job("", &arguments), &arguments[0], &arguments, || {
            starts += 1;
            anyhow::bail!("start was refused")
        });
        assert_eq!(starts, 1);
        assert_eq!(result.unwrap_err().to_string(), "start was refused");
    }

    #[test]
    #[cfg(unix)]
    fn default_menu_settings_never_replace_an_existing_unloaded_job() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("glass.plist");
        let saved = "existing custom settings on 127.0.0.1:7333";
        std::fs::write(&path, saved).unwrap();
        publish_launch_agent(&path, b"default settings on 127.0.0.1:7300").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn missing_menu_settings_are_published_completely_without_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("glass.plist");
        publish_launch_agent(&path, b"complete new settings").unwrap();
        publish_launch_agent(&path, b"different settings").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete new settings");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn unloaded_custom_menu_settings_are_read_without_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("glass.plist");
        let arguments = menu_arguments();
        let mut saved = plist::Dictionary::new();
        saved.insert("Label".into(), "tech.fixedwidth.glass".into());
        saved.insert(
            "ProgramArguments".into(),
            arguments
                .iter()
                .cloned()
                .map(plist::Value::String)
                .collect::<Vec<_>>()
                .into(),
        );
        plist::to_file_xml(&path, &saved).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            macos_impl::read_menu_arguments(&path, &arguments[0]).unwrap(),
            arguments
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        for (key, value) in [
            ("Program", "/Applications/Other.app/Contents/MacOS/other"),
            ("Label", "com.example.other"),
        ] {
            let mut changed = saved.clone();
            changed.insert(key.into(), value.into());
            plist::to_file_xml(&path, &changed).unwrap();
            assert!(
                macos_impl::read_menu_arguments(&path, &arguments[0]).is_err(),
                "{key}"
            );
        }
    }

    // --- run (non-macOS stub) -----------------------------------------------------------

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn run_fails_fast_off_macos() {
        let err = run(SetupArgs {
            account: None,
            non_interactive: false,
            launchagent: false,
            no_launchagent: false,
            addr: None,
        })
        .expect_err("setup must refuse to run off macOS");
        assert!(matches!(err, glass_core::GlassError::Backend(_)));
        assert!(err.to_string().contains("macOS-only"));
    }
}
