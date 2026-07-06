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
//! [`is_inside_app_bundle`], [`codesign_report_is_unstable`], and `parse_health_response`
//! are pure — no OS call, no IO — and are exercised here. (`parse_health_response` is a
//! plain code reference, not a doc link, since it's `#[cfg(any(target_os = "macos", test))]`
//! and so doesn't exist in scope for a plain non-test build on other platforms — a doc link
//! to it would be broken there.) The interactive grant flow itself (`#[cfg(target_os =
//! "macos")]` inside [`run`], plumbed through the private `macos_impl` submodule) is
//! macOS-only: permission prompts, `codesign`/`launchctl` shell-outs, real file writes.
//! `fetch_health` is likewise macOS-only (same reason it's a plain reference here) but
//! stays its own top-level `pub(crate) fn` rather than moving into `macos_impl`, since a
//! sibling onboarding module calls it too (see its doc comment).

// `GlassError` itself is only named in the `#[cfg(not(target_os = "macos"))]` arm of `run`
// (and its test) — on a macOS build that arm doesn't exist, so import only `Result` here
// and spell out `glass_core::GlassError` at its one use site to avoid an unused-import
// warning on that platform.
use std::path::Path;

use glass_core::Result;

// `HealthStatus` is only named by `parse_health_response` (below) and `fetch_health` (near
// `macos_impl`), which are themselves gated to macOS (the only platform with a running
// server to poll) plus `#[cfg(test)]` (so the parser stays Linux-testable) — gate this `use`
// the same way, or a plain non-test Linux/Windows build would warn on an import nothing in
// scope actually needs.
#[cfg(any(target_os = "macos", test))]
use crate::health::HealthStatus;

/// Extract and validate a [`HealthStatus`] out of a raw `/healthz` HTTP response: split off
/// the body at the blank line, require a `200` status line, then deserialize the body as
/// JSON. `None` for anything short of a genuinely healthy response — text that isn't HTTP at
/// all, a non-200 status, or a body that doesn't deserialize — never a fabricated
/// [`HealthStatus`]. Pure (no IO), so it's unit-tested here directly against literal
/// response text; [`fetch_health`] is the only real caller, feeding it a live socket read.
#[cfg(any(target_os = "macos", test))]
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
/// or fall back to a non-prompting default (`--non-interactive`). Pure — no OS call, no IO —
/// same Linux-testable shape as [`registration_line`]/[`fill_launch_agent`] above; the actual
/// prompt (when both flags are absent and the run is interactive) lives in the macOS-only
/// body of [`run`], since reading stdin isn't something to unit-test here.
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
/// warns, never refuses, since `setup` should still be usable from `cargo run` while
/// iterating. Pure (no filesystem access — just path-shape matching), so it's unit-tested on
/// Linux against fabricated paths.
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
#[cfg(any(target_os = "macos", test))]
fn app_bundle_path(exe: &Path) -> String {
    // `is_inside_app_bundle` guarantees the three-parents-up ancestor is the `*.app` dir.
    if is_inside_app_bundle(exe) {
        if let Some(bundle) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            return bundle.to_string_lossy().into_owned();
        }
    }
    DEFAULT_APP_PATH.to_string()
}

/// The parsed `setup` invocation, forwarded from the `Setup` clap variant (see `cli.rs`). A
/// struct rather than four positional arguments to [`run`] so the three adjacent `bool`s can't
/// be transposed at the call site without a field-name mismatch.
#[derive(Debug, Clone)]
pub struct SetupArgs {
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
        non_interactive,
        launchagent,
        no_launchagent,
        addr,
    } = args;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (non_interactive, launchagent, no_launchagent, addr);
        Err(glass_core::GlassError::Backend(
            "setup is macOS-only".into(),
        ))
    }
    #[cfg(target_os = "macos")]
    {
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
                     docs/running-on-macos.md). Then run `glass-mcp setup` again.",
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

/// Raw `GET /healthz` against the running server at `addr` (`host:port`), on the same
/// bounded-TCP-connect budget [`macos_impl::launch_agent_is_serving`] uses to confirm a
/// just-bootstrapped LaunchAgent is up. `None` for anything short of a full, parseable
/// response — an unresolvable `addr`, a refused/timed-out connect, a read timeout, or
/// [`parse_health_response`] rejecting the body — never a fabricated [`HealthStatus`].
///
/// `pub(crate)`, and kept at the top level rather than nested in the private `macos_impl`
/// module, because it has callers outside that module — `run`'s guided-enable poll (via
/// `macos_impl::guide_enable_and_verify` right here in `setup.rs`) and a later onboarding
/// module's dialog gate, both in this crate — and a private `mod` can't be named from a
/// sibling module regardless of its items' visibility. Crate-internal, so `pub(crate)`, not
/// `pub`.
#[cfg(target_os = "macos")]
pub(crate) fn fetch_health(addr: &str) -> Option<HealthStatus> {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let sock = addr.to_socket_addrs().ok()?.next()?;
    let mut stream =
        TcpStream::connect_timeout(&sock, macos_impl::LIVENESS_CONNECT_TIMEOUT).ok()?;
    stream
        .set_read_timeout(Some(macos_impl::LIVENESS_CONNECT_TIMEOUT))
        .ok()?;
    write!(
        stream,
        "GET /healthz HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    parse_health_response(&buf)
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
    /// `pub(super)`: also reused by [`super::fetch_health`] as its connect *and* read timeout
    /// (it isn't a poll loop like [`launch_agent_is_serving`], just a single bounded probe,
    /// so one budget covers both).
    pub(super) const LIVENESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
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
                 across a rebuild; see docs/running-on-macos.md.",
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
                         docs/running-on-macos.md#1-create-a-signing-identity).",
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
    /// check can read the matching field back off [`super::fetch_health`]).
    type GrantPane = (
        &'static str,
        &'static str,
        fn(&super::HealthStatus) -> Option<bool>,
    );

    /// The two macOS TCC panes `setup` guides the user through, paired with the `/healthz`
    /// field each maps to. Named once so [`guide_enable_and_verify`] both opens the panes and
    /// reads back the matching grant without re-typing the labels.
    fn grant_panes() -> [GrantPane; 2] {
        [
            (
                "Screen Recording",
                glass_macos::screen_recording_pane_url(),
                |h| h.screen_recording,
            ),
            (
                "Accessibility",
                glass_macos::accessibility_pane_url(),
                |h| h.accessibility,
            ),
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
        for (label, pane_url, _) in grant_panes() {
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
        for (label, _, field) in grant_panes() {
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

    /// Write the filled LaunchAgent plist to `~/Library/LaunchAgents/tech.fixedwidth.glass.plist`
    /// (creating it and `~/Library/Logs/GlassMcp/` if needed), (re)load it with `launchctl
    /// bootstrap gui/<uid>`, and confirm it's actually serving before reporting success.
    ///
    /// Returns `Ok(None)` once a bounded TCP connect to `addr` confirms the agent is accepting
    /// connections; `Ok(Some((label, instruction)))` when the job loaded but isn't serving, so
    /// the caller can fold it into the same pending / non-zero-exit path a missing grant uses
    /// (never a false "installed + started"); `Err` for a hard failure — no `HOME`, an
    /// un-writable plist, a drifted template, or `bootstrap` itself erroring.
    pub(super) fn install_launch_agent(
        app_bin: &str,
        addr: &str,
    ) -> Result<Option<(&'static str, String)>> {
        let home = std::env::var("HOME").map_err(|_| {
            GlassError::Backend("HOME is not set; can't resolve ~/Library/LaunchAgents".into())
        })?;
        let launch_agents_dir = Path::new(&home).join("Library/LaunchAgents");
        let logs_dir = Path::new(&home).join("Library/Logs/GlassMcp");
        std::fs::create_dir_all(&launch_agents_dir)?;
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
        let plist_path = launch_agents_dir.join("tech.fixedwidth.glass.plist");
        std::fs::write(&plist_path, filled)?;

        let uid = self_uid();
        let target = format!("gui/{uid}/tech.fixedwidth.glass");
        // Idempotent (re)load. `bootstrap` fails with "already loaded" on a second run — which
        // the flow itself asks for, since a Screen Recording grant only takes effect after a
        // relaunch — and wouldn't pick up an `--addr` change. Unload first, ignoring the exit
        // status (a harmless no-op when nothing is loaded), so every re-run converges.
        let _ = Command::new("launchctl")
            .arg("bootout")
            .arg(&target)
            .status();

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
        // came up serving: a port clash on `--addr` crash-loops under `KeepAlive=true` yet
        // bootstrap still returns success. Confirm real liveness before claiming success.
        if launch_agent_is_serving(addr) {
            println!(
                "\n  \u{2713} installed + started {target} ({})",
                plist_path.display()
            );
            Ok(None)
        } else {
            let instruction = format!(
                "LaunchAgent {target} loaded but isn't accepting connections on {addr} yet — \
                 check ~/Library/Logs/GlassMcp/stderr.log (a port clash on --addr crash-loops \
                 under KeepAlive), resolve it, then run `glass-mcp setup` again."
            );
            println!("\n  \u{2717} {instruction}");
            Ok(Some(("LaunchAgent", instruction)))
        }
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
            app_bundle_path(Path::new("/home/mpd/glass/target/release/glass-mcp")),
            DEFAULT_APP_PATH
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
            "/home/mpd/glass/target/release/glass-mcp"
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
        // via Keychain Access (see docs/running-on-macos.md#1-create-a-signing-identity).
        let report = "Executable=/Applications/GlassMcp.app/Contents/MacOS/glass-mcp\n\
                       Identifier=tech.fixedwidth.glass\n\
                       Format=Mach-O thin (arm64)\n\
                       CodeDirectory v=20400 size=411 flags=0x0(none) hashes=8+3 location=embedded\n\
                       Signature=DER encoded\n\
                       Authority=glass-mcp signing\n\
                       TeamIdentifier=not set\n";
        assert!(!codesign_report_is_unstable(report));
    }

    // --- run (non-macOS stub) -----------------------------------------------------------

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn run_fails_fast_off_macos() {
        let err = run(SetupArgs {
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
