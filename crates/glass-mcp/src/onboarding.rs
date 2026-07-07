//! Double-click first-run for GlassMcp.app. Launched by LaunchServices (see main.rs launch
//! routing), the app is its own TCC responsible process, so its grant requests raise the
//! standard popups attributed to GlassMcp.app. We then install the LaunchAgent, confirm via
//! /healthz, and show + copy the server's MCP endpoint — client-agnostic; per-client wiring is
//! documented, not assumed.

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
    /// Both grants confirmed via `/healthz` — hand off the endpoint.
    Ready(HealthStatus),
    /// The agent was reached, but a grant is still off — name the missing grant, no endpoint.
    GrantsMissing(HealthStatus),
    /// The agent never answered `/healthz` within the deadline — point at its log, no endpoint.
    Unreachable,
    /// The install reported the job loaded but not serving — carries the user-actionable
    /// instruction; no endpoint.
    NotServing(String),
}

/// The text shown in the completion dialog, one honest message per [`Outcome`]. Only a `Ready`
/// outcome hands off the MCP endpoint (and only claims a clipboard copy when `copied`);
/// `GrantsMissing` names the missing grant; `Unreachable` and `NotServing` point at the agent's
/// log / instruction without an endpoint (nothing to connect to yet). Pure (no IO), so it's
/// unit-tested on Linux; gated to macOS+test so a plain non-test Linux/Windows build doesn't
/// warn it dead.
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
            // Only claim the clipboard copy that actually happened (review finding M1).
            let clipboard_line = if copied {
                format!("MCP endpoint (Streamable HTTP), copied to your clipboard:\n{endpoint}")
            } else {
                format!("MCP endpoint (Streamable HTTP) — copy it from here:\n{endpoint}")
            };
            format!(
                "glass is ready — Screen Recording and Accessibility are granted.\n\n\
                 {clipboard_line}\n\n\
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

/// Onboarding entry: request grants as the self-responsible app, install the LaunchAgent,
/// confirm via the agent's `/healthz`, and present the completion dialog. macOS-only — a
/// no-op error off macOS, since there's no LaunchServices double-click hand-off to onboard
/// anywhere else.
pub fn run(addr: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let endpoint = format!("http://{addr}/");
        let exe = std::env::current_exe()?;
        // Reuse setup's validated bundle-path helper (checks the Contents/MacOS shape) rather
        // than a weaker local walk-up-to-any-`.app`.
        let app_path = crate::setup::app_bundle_path(&exe);

        // 1. Request grants from our own identity — the popups attribute to GlassMcp.app,
        // since a double-clicked .app is its own TCC responsible process (unlike a terminal
        // spawning `setup`, whose grant would key to the terminal instead).
        glass_macos::request_screen_recording();
        glass_macos::request_accessibility();

        // 2. Install the LaunchAgent (a fresh process that re-reads the grants), then resolve
        // which of the four real outcomes we're in. Install reporting `Some((_, instruction))`
        // means the job loaded but isn't serving; otherwise poll `/healthz` — a `None` there is
        // an unreachable agent (never a fabricated grants-denied status), a reached-but-not-ready
        // read is a genuine missing grant, and a ready read is the hand-off.
        let outcome = match crate::setup::install_launch_agent(&exe.to_string_lossy(), addr)? {
            Some((_, instruction)) => Outcome::NotServing(instruction),
            None => match poll_health_until_ready(addr) {
                Some(h) if h.grants_ready() => Outcome::Ready(h),
                Some(h) => Outcome::GrantsMissing(h),
                None => Outcome::Unreachable,
            },
        };

        // 3. Hand off: only a Ready outcome has an endpoint to copy — copy it and remember
        // whether the copy actually succeeded so the dialog never claims a copy that didn't
        // happen. The other outcomes have nothing to connect to yet.
        let copied = match &outcome {
            Outcome::Ready(_) => copy_to_clipboard(&endpoint).is_ok(),
            _ => false,
        };
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

/// Poll the LaunchAgent's `/healthz` (via [`crate::setup::fetch_health`]) until both grants
/// read ready or a minute elapses, returning the last read seen either way — a `None` read
/// (agent unreachable) never fabricates a status, it's simply skipped. macOS-only: the only
/// platform with a `/healthz` to poll.
#[cfg(target_os = "macos")]
fn poll_health_until_ready(addr: &str) -> Option<HealthStatus> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(h) = crate::setup::fetch_health(addr) {
            // `HealthStatus: Copy`, so recording the last read doesn't move it out of `h`.
            last = Some(h);
            if h.grants_ready() {
                return Some(h);
            }
        }
        std::thread::sleep(Duration::from_millis(750));
    }
    last
}

/// Copy `text` to the general pasteboard via `pbcopy` — no AppKit/NSPasteboard dependency
/// needed in this headless binary.
#[cfg(target_os = "macos")]
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
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
}
