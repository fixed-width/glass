//! Double-click first-run for GlassMcp.app. Launched by LaunchServices (see main.rs launch
//! routing), the app is its own TCC responsible process, so its grant requests raise the
//! standard popups attributed to GlassMcp.app.
//!
//! The flow: (A) if a healthy agent is already serving with both grants held, short-circuit —
//! copy the endpoint and hand off ("already running"), touching nothing. Otherwise (B) request
//! Accessibility then Screen Recording (AX first so Screen Recording's "Quit & Reopen" prompt
//! lands at the end), install + start the menu-bar LaunchAgent, open both Privacy panes, and
//! block on a guided modal until the user clicks OK; then (C) restart the agent once so it
//! re-reads TCC and verify via `/healthz` — showing + copying the server's MCP endpoint only
//! when both grants actually read ready. Client-agnostic; per-client wiring is documented, not
//! assumed.

// `HealthStatus` is only named by `completion_message` below, which is itself gated to
// macOS (the only platform this dialog flow runs on) plus `#[cfg(test)]` (so the pure
// builder stays Linux-testable) — gate this `use` the same way, or a plain non-test
// Linux/Windows build would warn on an import nothing in scope actually needs.
#[cfg(any(target_os = "macos", test))]
use crate::health::HealthStatus;

/// The address [`run`] binds the onboarded LaunchAgent to when the caller has no more specific
/// preference — a LaunchServices double-click never passes one. Not feature-gated (unlike
/// `crate::serve::config::DEFAULT_ADDR`, which only exists when the network-transport feature is
/// compiled in — a doc link to it would be broken without that feature, so this is a plain code
/// reference): onboarding always needs an address to hand to [`crate::setup`] regardless of which
/// optional features this build carries. Matches the shipped LaunchAgent plist template and the
/// other `127.0.0.1:7300` defaults in `setup.rs`/`serve/config.rs`.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7300";

/// The four terminal outcomes of the onboarding install + verify sequence, each mapped to an
/// honest completion message by [`completion_message`]. Distinguishing "reached but a grant is
/// off" from "never answered" and "loaded but not serving" is the whole point: flattening the
/// latter two into a fabricated grants-denied status told the user the wrong cause. Gated to
/// macOS+test (like [`completion_message`]/[`HealthStatus`] in this file) so the pure message
/// builder stays Linux-testable while the enum doesn't warn dead on a plain non-test build.
#[cfg(any(target_os = "macos", test))]
enum Outcome {
    /// Both grants confirmed via `/healthz` after the guided grant flow — hand off the endpoint.
    Ready(HealthStatus),
    /// A healthy agent was already serving with both grants held before we touched anything —
    /// short-circuit: hand off the endpoint without re-requesting grants or reinstalling.
    /// Distinct from [`Outcome::Ready`] only in wording ("already running"), so the returning
    /// user isn't told glass just became ready when it was ready all along.
    AlreadyRunning(HealthStatus),
    /// The agent was reached, but a grant is still off — name the missing grant, no endpoint.
    GrantsMissing(HealthStatus),
    /// The agent never answered `/healthz` within the deadline — point at its log, no endpoint.
    Unreachable,
    /// The install reported the job loaded but not serving — carries the user-actionable
    /// instruction; no endpoint.
    NotServing(String),
}

/// The endpoint hand-off line shared by the `Ready` and `AlreadyRunning` completion messages:
/// claims a clipboard copy only when `copied` actually succeeded (review finding M1 — never
/// report a copy that didn't happen), otherwise tells the user to copy it from the dialog.
/// Pure; gated macOS+test like its callers.
#[cfg(any(target_os = "macos", test))]
fn endpoint_handoff_line(endpoint: &str, copied: bool) -> String {
    if copied {
        format!("MCP endpoint (Streamable HTTP), copied to your clipboard:\n{endpoint}")
    } else {
        format!("MCP endpoint (Streamable HTTP) — copy it from here:\n{endpoint}")
    }
}

/// The text shown in the completion dialog, one honest message per [`Outcome`]. The two ready
/// outcomes (`Ready` after the guided flow, `AlreadyRunning` for an already-healthy agent) hand
/// off the MCP endpoint (and only claim a clipboard copy when `copied`); `GrantsMissing` names
/// the missing grant; `Unreachable` and `NotServing` point at the agent's log / instruction
/// without an endpoint (nothing to connect to yet). Pure (no IO), so it's unit-tested on Linux;
/// gated to macOS+test so a plain non-test Linux/Windows build doesn't warn it dead.
#[cfg(any(target_os = "macos", test))]
fn completion_message(outcome: &Outcome, endpoint: &str, app_path: &str, copied: bool) -> String {
    match outcome {
        Outcome::Ready(h) => {
            // `Ready` is only ever built once `/healthz` confirmed both grants; assert the
            // invariant so a future refactor can't route a not-ready status down the
            // "glass is ready" path. (Reads `h`, so its field isn't dead code either.)
            debug_assert!(
                h.grants_ready(),
                "Outcome::Ready must carry a grants-ready HealthStatus"
            );
            let endpoint_line = endpoint_handoff_line(endpoint, copied);
            // Append the optional CLI symlink: a .dmg install drops `glass-mcp` inside the
            // bundle, never on `$PATH`, so offer (never require) a symlink for terminal use.
            // Derived from `app_path` so a non-default install location stays correct.
            format!(
                "glass is ready — Screen Recording and Accessibility are granted.\n\n\
                 {endpoint_line}\n\n\
                 Add it to your MCP client to start driving apps.\n\n\
                 Optional — glass-mcp isn't on your PATH from a .dmg; to run it in a terminal:\n\
                 sudo ln -s {app_path}/Contents/MacOS/glass-mcp /usr/local/bin/glass-mcp"
            )
        }
        // Same hand-off as `Ready`, but the agent was already serving with both grants before
        // onboarding did anything — so say "already running" rather than implying this run just
        // enabled it. No CLI symlink hint: a returning user has already been through setup.
        Outcome::AlreadyRunning(h) => {
            debug_assert!(
                h.grants_ready(),
                "Outcome::AlreadyRunning must carry a grants-ready HealthStatus"
            );
            let endpoint_line = endpoint_handoff_line(endpoint, copied);
            format!(
                "glass is already running — Screen Recording and Accessibility are granted.\n\n\
                 {endpoint_line}\n\n\
                 Add it to your MCP client to start driving apps."
            )
        }
        Outcome::GrantsMissing(h) => {
            let mut missing = Vec::new();
            if h.screen_recording != Some(true) {
                missing.push("Screen Recording");
            }
            if h.accessibility != Some(true) {
                missing.push("Accessibility");
            }
            format!(
                "glass still needs: {}.\n\nEnable GlassMcp.app in System Settings → \
                 Privacy & Security for each, then re-open GlassMcp.app.\nApp: {app_path}",
                missing.join(" and ")
            )
        }
        // Distinct from GrantsMissing: the agent never answered, so the cause isn't a denied
        // grant. Don't enumerate grants and don't hand off an endpoint — point at the log.
        Outcome::Unreachable => {
            "glass installed its background agent, but it didn't start serving within the \
             timeout. Its LaunchAgent log (Console → user log, or ~/Library/Logs) will show \
             why; then re-open GlassMcp.app."
                .to_string()
        }
        // The install already knows why it isn't serving (e.g. a port clash) — surface that
        // actionable instruction verbatim rather than guessing a cause.
        Outcome::NotServing(instruction) => format!(
            "glass installed its background agent, but it isn't serving yet:\n{instruction}"
        ),
    }
}

/// The blocking guided-grant modal shown after both Privacy panes are opened: names *both*
/// grants (Screen Recording to see the screen, Accessibility to move the mouse and type),
/// tells the user to enable **GlassMcp.app** in each, and — crucially — pre-empts macOS's
/// "Quit & Reopen" prompt that a Screen Recording grant raises, telling them to click **Later**
/// because glass restarts its own background agent once they click OK. Deliberately carries no
/// endpoint and no CLI symlink hint: this is the *instruction* dialog, shown before the restart;
/// the endpoint (and the optional symlink) only appear in the [`completion_message`] afterward,
/// once `/healthz` has actually confirmed the grants. Pure (no IO); gated macOS+test so it stays
/// Linux-unit-testable while a plain non-test build doesn't warn it dead.
#[cfg(any(target_os = "macos", test))]
fn grant_modal_text(app_path: &str) -> String {
    format!(
        "glass needs two macOS permissions to see and drive other apps:\n\n\
         • Screen Recording — to see the screen\n\
         • Accessibility — to move the mouse and type\n\n\
         Both System Settings panes are now open. Enable GlassMcp.app in each (toggle it on if \
         listed, otherwise click ＋ and add it):\n{app_path}\n\n\
         If macOS offers to \"Quit & Reopen\" for Screen Recording, click Later — glass restarts \
         its own background agent for you.\n\n\
         Click OK here once both are enabled."
    )
}

/// Onboarding entry (see the module doc for the full A/B/C flow): short-circuit if an agent is
/// already serving with both grants; otherwise request grants as the self-responsible app,
/// install the LaunchAgent, guide the user through the two Privacy panes with a blocking modal,
/// then restart the agent once and verify via `/healthz` before presenting the completion
/// dialog. macOS-only — a no-op error off macOS, since there's no LaunchServices double-click
/// hand-off to onboard anywhere else.
pub fn run(addr: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let endpoint = format!("http://{addr}/");
        let exe = std::env::current_exe()?;
        // Reuse setup's validated bundle-path helper (checks the Contents/MacOS shape) rather
        // than a weaker local walk-up-to-any-`.app`.
        let app_path = crate::setup::app_bundle_path(&exe);

        // A. Already running? If a healthy agent is already serving with both grants held, this
        // is a returning user: copy the endpoint and hand off immediately. Don't re-request
        // grants (would pop dialogs for nothing) or reinstall over a working agent.
        if let Some(h) = crate::setup::fetch_health(addr) {
            if h.grants_ready() {
                let copied = copy_to_clipboard(&endpoint).is_ok();
                let msg =
                    completion_message(&Outcome::AlreadyRunning(h), &endpoint, &app_path, copied);
                show_dialog(&msg);
                return Ok(());
            }
        }

        // B. Request both grants as GlassMcp.app itself — a double-clicked .app is its own TCC
        // responsible process, so the popups attribute to GlassMcp.app (a terminal spawning
        // `setup` would instead key the grant to the terminal). Accessibility first: a Screen
        // Recording grant can raise macOS's "Quit & Reopen" prompt, so requesting AX first keeps
        // that interruption isolated to the end of the sequence.
        glass_macos::request_accessibility();
        glass_macos::request_screen_recording();

        // Install + start the menu-bar LaunchAgent (the fresh observer/server that re-reads the
        // grants). A job that loaded but isn't serving is an outstanding action, not a success:
        // surface its actionable instruction and stop, rather than restarting a dead agent (a
        // restart can't fix e.g. a port clash) or claiming a hand-off we can't make.
        if let Some((_, instruction)) =
            crate::setup::install_launch_agent(&exe.to_string_lossy(), addr)?
        {
            let msg = completion_message(
                &Outcome::NotServing(instruction),
                &endpoint,
                &app_path,
                false,
            );
            show_dialog(&msg);
            return Ok(());
        }

        // Open both Privacy panes, then block on the guided modal until the user clicks OK.
        let _ = glass_macos::open_pane(glass_macos::accessibility_pane_url());
        let _ = glass_macos::open_pane(glass_macos::screen_recording_pane_url());
        show_dialog(&grant_modal_text(&app_path));

        // C. On OK: one restart so the agent re-reads TCC (the Screen Recording grant is cached
        // per-process at launch), then verify via `/healthz`. `kickstart -k` returns as soon as
        // launchd *spawns* the new process, not once it's listening — the fresh agent still has
        // to build its tokio runtime, do AppKit init, and bind the socket, which takes a few
        // hundred ms. Probing once immediately races that startup window and reports a bogus
        // `Unreachable` on an otherwise-successful grant+restart, so bounded-poll for the first
        // reachable read instead. That first reachable read is authoritative (a freshly-started
        // process reads TCC fresh) and final: don't keep polling for grants to flip — the user
        // already clicked OK, so a reached-but-not-ready read is a genuine missing grant, not a
        // race to retry.
        crate::setup::restart_launch_agent()?;
        let outcome = match poll_health_until_reachable(addr) {
            Some(h) if h.grants_ready() => Outcome::Ready(h),
            Some(h) => Outcome::GrantsMissing(h),
            None => Outcome::Unreachable,
        };
        let copied = matches!(outcome, Outcome::Ready(_)) && copy_to_clipboard(&endpoint).is_ok();
        let msg = completion_message(&outcome, &endpoint, &app_path, copied);
        show_dialog(&msg);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = addr;
        anyhow::bail!("onboarding is macOS-only")
    }
}

/// Poll the LaunchAgent's `/healthz` (via [`crate::setup::fetch_health`]) until the first
/// reachable read or a ~10s budget elapses. Bounded to cover `kickstart -k`'s launchd-startup
/// window (see the call site in [`run`]) — once the agent answers at all, that read's grant
/// state is final: the caller classifies it (ready vs. grants-missing) rather than this helper
/// looping until grants flip. macOS-only: the only platform with a `/healthz` to poll.
#[cfg(target_os = "macos")]
fn poll_health_until_reachable(addr: &str) -> Option<HealthStatus> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(h) = crate::setup::fetch_health(addr) {
            return Some(h);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Copy `text` to the general pasteboard via `pbcopy` — no AppKit/NSPasteboard dependency
/// needed in this headless binary. `pub(crate)` so the menu-bar app's "Copy endpoint" item
/// (`crate::menubar`) reuses this exact helper rather than rolling its own `pbcopy` shell-out.
#[cfg(target_os = "macos")]
pub(crate) fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("Stdio::piped guarantees a stdin handle")
        .write_all(text.as_bytes())?;
    child.wait()?;
    Ok(())
}

/// Show `message` in a modal `osascript` dialog. `osascript` keeps this headless binary free
/// of an AppKit/event-loop dependency. The message is passed via `argv` (`item 1 of argv`),
/// not string-interpolated into the script text, so it can't break out of the AppleScript
/// string it's placed into.
#[cfg(target_os = "macos")]
fn show_dialog(message: &str) {
    use std::process::Command;
    let script = r#"on run(argv)
    display dialog (item 1 of argv) with title "glass" buttons {"OK"} default button "OK"
end run"#;
    // Best-effort: if osascript can't run (or exits non-zero), a double-click would otherwise
    // complete with no visible output at all (review finding M2) — fall back to stderr so the
    // message is never silently lost.
    let shown = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .arg(message)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !shown {
        eprintln!("glass onboarding:\n{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::HealthStatus;

    const ENDPOINT: &str = "http://127.0.0.1:7300/";
    const APP: &str = "/Applications/GlassMcp.app";

    fn both_granted() -> HealthStatus {
        HealthStatus {
            ok: true,
            screen_recording: Some(true),
            accessibility: Some(true),
        }
    }

    #[test]
    fn ready_copied_shows_endpoint_and_says_copied() {
        let m = completion_message(&Outcome::Ready(both_granted()), ENDPOINT, APP, true);
        assert!(m.contains(ENDPOINT));
        assert!(m.to_lowercase().contains("ready") || m.to_lowercase().contains("granted"));
        assert!(m.to_lowercase().contains("copied to your clipboard"));
        // client-agnostic: no assumption of a specific MCP client tool
        assert!(!m.to_lowercase().contains("claude mcp add"));
    }

    #[test]
    fn ready_not_copied_shows_endpoint_without_claiming_a_copy() {
        let m = completion_message(&Outcome::Ready(both_granted()), ENDPOINT, APP, false);
        assert!(m.contains(ENDPOINT));
        assert!(m.contains("copy it from here"));
        // don't claim a copy that didn't happen (review finding M1)
        assert!(!m.to_lowercase().contains("copied to your clipboard"));
        assert!(!m.to_lowercase().contains("claude mcp add"));
    }

    #[test]
    fn grants_missing_names_the_grant_and_shows_no_endpoint() {
        let h = HealthStatus {
            ok: true,
            screen_recording: Some(true),
            accessibility: Some(false),
        };
        let m = completion_message(&Outcome::GrantsMissing(h), ENDPOINT, APP, false);
        assert!(m.contains("Accessibility"));
        // don't hand off an endpoint that isn't ready
        assert!(!m.contains(ENDPOINT));
    }

    #[test]
    fn unreachable_shows_no_endpoint_and_does_not_blame_grants() {
        let m = completion_message(&Outcome::Unreachable, ENDPOINT, APP, false);
        assert!(!m.contains(ENDPOINT));
        // the wrong-cause bug: an unreachable agent must NOT be reported as missing grants
        assert!(!m.contains("Screen Recording"));
        assert!(!m.contains("Accessibility"));
    }

    #[test]
    fn not_serving_surfaces_the_instruction_and_shows_no_endpoint() {
        let instruction =
            "LaunchAgent tech.fixedwidth.glass loaded but isn't accepting connections \
             on 127.0.0.1:7300 yet — check ~/Library/Logs/GlassMcp/stderr.log"
                .to_string();
        let m = completion_message(
            &Outcome::NotServing(instruction.clone()),
            ENDPOINT,
            APP,
            false,
        );
        assert!(m.contains(&instruction));
        assert!(!m.contains(ENDPOINT));
    }

    // --- completion_message: AlreadyRunning ----------------------------------------------

    #[test]
    fn already_running_copied_shows_endpoint_and_says_already_running() {
        let m = completion_message(
            &Outcome::AlreadyRunning(both_granted()),
            ENDPOINT,
            APP,
            true,
        );
        assert!(m.contains(ENDPOINT));
        assert!(m.to_lowercase().contains("already running"));
        assert!(m.to_lowercase().contains("copied to your clipboard"));
        assert!(!m.to_lowercase().contains("claude mcp add"));
    }

    #[test]
    fn already_running_not_copied_shows_endpoint_without_claiming_a_copy() {
        let m = completion_message(
            &Outcome::AlreadyRunning(both_granted()),
            ENDPOINT,
            APP,
            false,
        );
        assert!(m.contains(ENDPOINT));
        assert!(m.contains("copy it from here"));
        // don't claim a copy that didn't happen (review finding M1)
        assert!(!m.to_lowercase().contains("copied to your clipboard"));
    }

    // --- completion_message: Ready CLI one-liner -----------------------------------------

    #[test]
    fn ready_dialog_offers_optional_cli_symlink() {
        let m = completion_message(&Outcome::Ready(both_granted()), ENDPOINT, APP, true);
        // the exact symlink one-liner (glass-mcp isn't on PATH from a .dmg), framed as optional
        assert!(m.contains(
            "sudo ln -s /Applications/GlassMcp.app/Contents/MacOS/glass-mcp \
             /usr/local/bin/glass-mcp"
        ));
        assert!(m.to_lowercase().contains("optional"));
        assert!(m.contains(ENDPOINT));
    }

    // --- grant_modal_text ----------------------------------------------------------------

    #[test]
    fn grant_modal_names_both_grants_and_warns_about_quit_reopen() {
        let m = grant_modal_text(APP);
        assert!(m.contains("Accessibility") && m.contains("Screen Recording"));
        // pre-empts macOS's "Quit & Reopen" Screen-Recording prompt: tell the user to click Later
        assert!(m.to_lowercase().contains("later"));
        assert!(m.contains("Quit & Reopen"));
        // names the bundle so the user can add it with the pane's ＋
        assert!(m.contains(APP));
    }

    #[test]
    fn grant_modal_omits_the_cli_symlink_hint() {
        // the optional CLI one-liner belongs in the Ready completion dialog, not the grant modal
        let m = grant_modal_text(APP);
        assert!(!m.contains("/usr/local/bin/glass-mcp"));
    }
}
