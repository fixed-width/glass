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

/// The text shown in the completion dialog. When both grants are in, it hands off the MCP
/// endpoint; when one is missing, it names the missing grant instead (no endpoint — nothing
/// to connect to yet). Pure (no IO), so it's unit-tested on Linux; gated to macOS+test so a
/// plain non-test Linux/Windows build doesn't warn it dead.
#[cfg(any(target_os = "macos", test))]
fn completion_message(h: &HealthStatus, endpoint: &str, app_path: &str) -> String {
    if h.grants_ready() {
        format!(
            "glass is ready — Screen Recording and Accessibility are granted.\n\n\
             MCP endpoint (Streamable HTTP), copied to your clipboard:\n{endpoint}\n\n\
             Add it to your MCP client to start driving apps."
        )
    } else {
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
}

/// Onboarding entry: request grants as the self-responsible app, install the LaunchAgent,
/// confirm via the agent's `/healthz`, and present the completion dialog. macOS-only — a
/// no-op error off macOS, since there's no LaunchServices double-click hand-off to onboard
/// anywhere else.
pub fn run(addr: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let endpoint = format!("http://{addr}/");
        let app_path =
            current_app_bundle_path().unwrap_or_else(|| "/Applications/GlassMcp.app".to_string());

        // 1. Request grants from our own identity — the popups attribute to GlassMcp.app,
        // since a double-clicked .app is its own TCC responsible process (unlike a terminal
        // spawning `setup`, whose grant would key to the terminal instead).
        glass_macos::request_screen_recording();
        glass_macos::request_accessibility();

        // 2. Install the LaunchAgent (a fresh process that re-reads the grants).
        let exe = std::env::current_exe()?;
        crate::setup::install_launch_agent(&exe.to_string_lossy(), addr)?;

        // 3. Confirm via the agent's /healthz (bounded poll) — never assume the grants took;
        // an unreachable agent reads as not-ready, same as the two Screen Recording /
        // Accessibility fields it would otherwise report.
        let health = poll_health_until_ready(addr).unwrap_or(HealthStatus {
            ok: false,
            screen_recording: None,
            accessibility: None,
        });

        // 4. Hand off: copy the endpoint (only when both grants are confirmed ready) and
        // show the dialog either way.
        if health.grants_ready() {
            let _ = copy_to_clipboard(&endpoint);
        }
        let msg = completion_message(&health, &endpoint, &app_path);
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
            last = Some(h.clone());
            if h.grants_ready() {
                return Some(h);
            }
        }
        std::thread::sleep(Duration::from_millis(750));
    }
    last
}

/// Walk up from the running exe (`…/GlassMcp.app/Contents/MacOS/glass-mcp`) to the enclosing
/// `*.app` directory. `None` when the exe isn't inside a bundle (e.g. a bare `cargo run`);
/// [`run`] falls back to the default install path in that case.
#[cfg(target_os = "macos")]
fn current_app_bundle_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mut p = exe.as_path();
    while let Some(parent) = p.parent() {
        if parent.extension().is_some_and(|e| e == "app") {
            return Some(parent.to_string_lossy().into_owned());
        }
        p = parent;
    }
    None
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
    let _ = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .arg(message)
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::HealthStatus;

    #[test]
    fn completion_message_ready_shows_endpoint_not_a_client_command() {
        let h = HealthStatus {
            ok: true,
            screen_recording: Some(true),
            accessibility: Some(true),
        };
        let m = completion_message(&h, "http://127.0.0.1:7300/", "/Applications/GlassMcp.app");
        assert!(m.contains("http://127.0.0.1:7300/"));
        assert!(m.to_lowercase().contains("ready") || m.to_lowercase().contains("granted"));
        // client-agnostic: no assumption of a specific MCP client tool
        assert!(!m.to_lowercase().contains("claude mcp add"));
    }

    #[test]
    fn completion_message_missing_grant_names_it() {
        let h = HealthStatus {
            ok: true,
            screen_recording: Some(true),
            accessibility: Some(false),
        };
        let m = completion_message(&h, "http://127.0.0.1:7300/", "/Applications/GlassMcp.app");
        assert!(m.contains("Accessibility"));
        assert!(!m.contains("http://127.0.0.1:7300/")); // don't hand off an endpoint that isn't ready
    }
}
