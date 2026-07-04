//! `glass-mcp setup`: the guided macOS first-run — request the two TCC grants (Screen
//! Recording, Accessibility), install the chosen run integration (an unattended
//! `gui/<uid>` LaunchAgent serving HTTP, or nothing for an attended/stdio client-spawned
//! run), and confirm with `doctor` plus a ready-to-paste MCP-client registration line.
//!
//! This module is split so the parts that don't need macOS are unit-testable on Linux:
//! [`RunMode`], [`registration_line`], [`fill_launch_agent`], [`run_mode_from_flags`],
//! [`is_inside_app_bundle`], and [`codesign_report_is_unstable`] are pure — no OS call, no
//! IO — and are exercised here. The interactive grant flow itself
//! (`#[cfg(target_os = "macos")]` inside [`run`], plumbed through the private `macos_impl`
//! submodule) is macOS-only: permission prompts, `codesign`/`launchctl` shell-outs, real
//! file writes.

// `GlassError` itself is only named in the `#[cfg(not(target_os = "macos"))]` arm of `run`
// (and its test) — on a macOS build that arm doesn't exist, so import only `Result` here
// and spell out `glass_core::GlassError` at its one use site to avoid an unused-import
// warning on that platform.
use std::path::Path;

use glass_core::Result;

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
                    "note: the console session is locked/asleep. That doesn't block \
                     requesting permissions below, but capture/input won't work until it's \
                     unlocked (`caffeinate -d` keeps the display awake, no sudo needed)."
                );
            }
            glass_macos::SessionState::Unlocked => {}
        }

        // Step 2: request the two TCC grants, one at a time. `Some((label, instruction))`
        // for a grant that's still missing once the poll times out; `None` once `granted()`
        // itself confirms it — never claimed on the user's say-so alone.
        println!("\nRequesting permissions:");
        let screen_recording = macos_impl::ensure_granted(
            "Screen Recording",
            glass_macos::screen_recording_pane_url(),
            glass_macos::screen_recording_remedy(),
            glass_macos::screen_recording_granted,
            glass_macos::request_screen_recording,
            true, // needs_relaunch_note: SR only takes effect for this process after a relaunch
            non_interactive,
        );
        let accessibility = macos_impl::ensure_granted(
            "Accessibility",
            glass_macos::accessibility_pane_url(),
            glass_macos::accessibility_remedy(),
            glass_macos::accessibility_granted,
            glass_macos::request_accessibility,
            false,
            non_interactive,
        );
        let mut pending: Vec<(&'static str, String)> = [screen_recording, accessibility]
            .into_iter()
            .flatten()
            .collect();

        // Step 3: pick the run mode and, for the unattended LaunchAgent, install it.
        let mode = match run_mode_from_flags(launchagent, no_launchagent) {
            Some(mode) => mode,
            None if non_interactive => RunMode::Stdio, // no prompts allowed; least-invasive default
            None => macos_impl::prompt_run_mode(),
        };
        let app_bin = exe.to_string_lossy().into_owned();
        let addr = addr.unwrap_or_else(|| macos_impl::DEFAULT_ADDR.to_string());
        match mode {
            // A LaunchAgent that loaded but isn't yet serving is an outstanding action, not a
            // success: fold it into the same `pending` / non-zero-exit path a missing grant
            // uses rather than printing a false "installed + started" (no-silent-success).
            RunMode::Http => {
                if let Some(item) = macos_impl::install_launch_agent(&app_bin, &addr)? {
                    pending.push(item);
                }
            }
            RunMode::Stdio => println!(
                "\nNot installing the LaunchAgent (attended/stdio). If one is already loaded \
                 from a previous run, remove it with: launchctl bootout gui/{}/tech.fixedwidth.glass",
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

    /// Ask a yes/no question on stdin; any I/O failure (no controlling terminal, EOF, ...)
    /// answers `false` rather than blocking or panicking.
    fn prompt_yes_no(question: &str) -> bool {
        print!("{question} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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

    /// Request one TCC grant if it isn't already held: call `request` (pops the consent
    /// dialog / adds glass to the pane), open the relevant System Settings pane, then poll
    /// `granted` for up to [`POLL_TIMEOUT`]. Returns `None` the moment `granted()` itself
    /// reports success (live-rechecked, never assumed); otherwise `Some((label,
    /// instruction))` naming the still-missing permission and what to do about it.
    ///
    /// `needs_relaunch_note` is `true` only for Screen Recording: unlike Accessibility, a
    /// Screen Recording grant only takes effect for *this* process after it's relaunched, so
    /// a still-ungranted read at the poll deadline doesn't necessarily mean the user didn't
    /// act — ask (interactively) or assume (`--non-interactive`, since there's no one to
    /// ask) that they did, and say so explicitly rather than reporting a plain "not granted".
    pub(super) fn ensure_granted(
        label: &'static str,
        pane_url: &str,
        remedy: &str,
        granted: impl Fn() -> bool,
        request: impl FnOnce() -> bool,
        needs_relaunch_note: bool,
        non_interactive: bool,
    ) -> Option<(&'static str, String)> {
        if granted() {
            println!("  \u{2713} {label}: already granted");
            return None;
        }
        if request() {
            println!("  \u{2713} {label}: granted");
            return None;
        }
        if let Err(e) = glass_macos::open_pane(pane_url) {
            eprintln!(
                "  note: couldn't open System Settings automatically ({e}); open Privacy & \
                 Security > {label} manually."
            );
        }
        let landed = poll_until(&granted, POLL_INTERVAL, POLL_TIMEOUT, || {
            println!("  waiting for you to enable glass in the {label} pane…");
        });
        if landed {
            println!("  \u{2713} {label}: granted");
            return None;
        }
        if needs_relaunch_note {
            let acted = non_interactive
                || prompt_yes_no(&format!("Did you enable {label} in System Settings?"));
            if acted {
                let instruction = format!(
                    "{label} changes take effect after a relaunch — enable glass, then run \
                     `glass-mcp setup` again."
                );
                println!("  \u{2717} {instruction}");
                return Some((label, instruction));
            }
        }
        let instruction =
            format!("{label}: not granted — {remedy}, then run `glass-mcp setup` again.");
        println!("  \u{2717} {instruction}");
        Some((label, instruction))
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
