//! `glass-mcp setup`: the guided macOS first-run — request the two TCC grants (Screen
//! Recording, Accessibility), install the chosen run integration (an unattended
//! `gui/<uid>` LaunchAgent serving HTTP, or nothing for an attended/stdio client-spawned
//! run), and confirm with `doctor` plus a ready-to-paste MCP-client registration line.
//!
//! This module is split so the parts that don't need macOS are unit-testable on Linux:
//! [`RunMode`], [`registration_line`], and [`fill_launch_agent`] are pure — no OS call, no
//! IO — and are exercised here. The interactive grant flow itself
//! (`#[cfg(target_os = "macos")]` inside [`run`]) is a stub in this task; it lands in a
//! follow-on task, macOS-only (permission prompts, `launchctl`, file writes).

// `GlassError` itself is only named in the `#[cfg(not(target_os = "macos"))]` arm of `run`
// (and its test) — on a macOS build that arm doesn't exist, so import only `Result` here
// and spell out `glass_core::GlassError` at its one use site to avoid an unused-import
// warning on that platform.
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

/// Fill the LaunchAgent plist template (`packaging/macos/tech.fixedwidth.glass.plist`):
/// substitute the app-binary path, the HTTP bind address, and the home directory the two
/// log paths are rooted under. `template` is the shipped plist text; returns the
/// ready-to-write plist. Pure string substitution — no IO, so the caller decides where (or
/// whether) to write the result.
pub fn fill_launch_agent(template: &str, app_bin: &str, addr: &str, home: &str) -> String {
    template
        .replace("/Applications/GlassMcp.app/Contents/MacOS/glass-mcp", app_bin)
        .replace("127.0.0.1:7300", addr)
        .replace("/Users/YOU", home)
}

/// Run `glass-mcp setup`. macOS-only: everywhere else this fails fast with an actionable
/// error rather than pretending to do something.
///
/// The flags mirror the `Setup` clap variant verbatim (see `cli.rs`) so the macOS body can
/// use them without re-threading the signature: `non_interactive` fails instead of
/// prompting (scripting/CI); `launchagent`/`no_launchagent` force the run mode instead of
/// asking; `addr` overrides the LaunchAgent's HTTP bind address.
pub fn run(non_interactive: bool, launchagent: bool, no_launchagent: bool, addr: Option<String>) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (non_interactive, launchagent, no_launchagent, addr);
        Err(glass_core::GlassError::Backend("setup is macOS-only".into()))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (non_interactive, launchagent, no_launchagent, addr);
        // The interactive grant flow (request Screen Recording + Accessibility, open the
        // relevant pane, poll for the grant, decide/install the run mode, confirm via
        // `doctor` + `registration_line`) is not implemented yet — it lands in a follow-on
        // task. This stub only proves the dispatch wiring and the macOS build.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- registration_line ------------------------------------------------------------

    #[test]
    fn stdio_registration_line_names_the_binary() {
        let line = registration_line(RunMode::Stdio, "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp", "127.0.0.1:7300");
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
        let filled = fill_launch_agent(TEMPLATE, "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp", "127.0.0.1:7300", "/Users/alice");
        assert!(filled.contains("/Applications/GlassMcp.app/Contents/MacOS/glass-mcp"));
    }

    #[test]
    fn fill_launch_agent_substitutes_a_custom_app_binary() {
        let filled = fill_launch_agent(TEMPLATE, "/opt/glass/glass-mcp", "127.0.0.1:7300", "/Users/alice");
        assert!(filled.contains("/opt/glass/glass-mcp"));
        assert!(!filled.contains("/Applications/GlassMcp.app"));
    }

    #[test]
    fn fill_launch_agent_substitutes_the_addr() {
        let filled = fill_launch_agent(TEMPLATE, "/opt/glass/glass-mcp", "0.0.0.0:9999", "/Users/alice");
        assert!(filled.contains("0.0.0.0:9999"));
        assert!(!filled.contains("127.0.0.1:7300"));
    }

    #[test]
    fn fill_launch_agent_substitutes_the_home_in_both_log_paths() {
        let filled = fill_launch_agent(TEMPLATE, "/opt/glass/glass-mcp", "127.0.0.1:7300", "/Users/alice");
        assert!(filled.contains("/Users/alice/Library/Logs/GlassMcp/stdout.log"));
        assert!(filled.contains("/Users/alice/Library/Logs/GlassMcp/stderr.log"));
        assert!(!filled.contains("/Users/YOU"));
    }

    // --- run (non-macOS stub) -----------------------------------------------------------

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn run_fails_fast_off_macos() {
        let err = run(false, false, false, None).expect_err("setup must refuse to run off macOS");
        assert!(matches!(err, glass_core::GlassError::Backend(_)));
        assert!(err.to_string().contains("macOS-only"));
    }
}
