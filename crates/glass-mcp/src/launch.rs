//! Launch-mode routing for the no-subcommand case. A LaunchServices launch of GlassMcp.app
//! (double-click / `open -a`) sets `__CFBundleIdentifier` to OUR bundle id and gives the
//! process a non-pipe stdin (`/dev/null`); an MCP client spawns the binary with a pipe stdin
//! and never sets our bundle id. We route to onboarding ONLY when BOTH signals hold, and fail
//! safe to stdio otherwise, so a client spawn — including one whose environment happens to
//! carry a stray `__CFBundleIdentifier` — is never hijacked. (Signals confirmed on-box against
//! a real LaunchServices double-click and a real MCP-client stdio spawn.)

/// LaunchServices sets this on a launched `.app`'s process; an MCP client's stdio spawn never
/// does. macOS/test-only: its only reader, [`classify_no_arg_launch`], is gated the same way.
#[cfg(any(target_os = "macos", test))]
const OUR_BUNDLE_ID: &str = "tech.fixedwidth.glass";

/// The two ways an invocation with no subcommand can be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoArgLaunch {
    /// A LaunchServices double-click / `open -a`: run the self-responsible onboarding flow.
    /// Only ever constructed by [`classify_no_arg_launch`], which is macOS/test-only — a plain
    /// non-test build on any other platform never constructs this variant, which would
    /// otherwise warn dead-code.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Onboarding,
    /// Anything else — including every non-macOS platform, which has no LaunchServices concept
    /// at all: serve MCP over stdio, same as before this routing existed.
    StdioServe,
}

/// Pure decision from the two measured signals. Onboarding requires our bundle id AND a
/// non-pipe stdin — either alone is not enough, so a foreign/absent bundle id, or a pipe
/// stdin regardless of bundle id, always falls back to stdio rather than risking hijacking an
/// MCP client's spawn.
#[cfg(any(target_os = "macos", test))]
pub fn classify_no_arg_launch(cfbundle_id: Option<&str>, stdin_is_pipe: bool) -> NoArgLaunch {
    if cfbundle_id == Some(OUR_BUNDLE_ID) && !stdin_is_pipe {
        NoArgLaunch::Onboarding
    } else {
        NoArgLaunch::StdioServe
    }
}

/// Read the real signals and classify. On non-macOS there's no LaunchServices concept at all,
/// so it's unconditionally stdio.
pub fn detect_no_arg_launch() -> NoArgLaunch {
    #[cfg(target_os = "macos")]
    {
        let id = std::env::var("__CFBundleIdentifier").ok();
        classify_no_arg_launch(id.as_deref(), stdin_is_pipe())
    }
    #[cfg(not(target_os = "macos"))]
    {
        NoArgLaunch::StdioServe
    }
}

/// `true` when stdin is a FIFO (pipe) — the MCP stdio transport. A LaunchServices double-click
/// gives the process `/dev/null` (a character device, not a FIFO), so this is the second half
/// of the discriminator. Uses `rustix::fs::fstat` (a safe syscall wrapper) rather than a raw
/// `libc::fstat`, per this repo's unsafe policy; an fstat failure reads as "not a pipe", so
/// the bundle-id half of [`classify_no_arg_launch`] alone decides.
#[cfg(target_os = "macos")]
fn stdin_is_pipe() -> bool {
    match rustix::fs::fstat(std::io::stdin()) {
        Ok(stat) => {
            rustix::fs::FileType::from_raw_mode(stat.st_mode as rustix::fs::RawMode).is_fifo()
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_bundle_id_and_not_a_pipe_is_onboarding() {
        assert_eq!(
            classify_no_arg_launch(Some("tech.fixedwidth.glass"), false),
            NoArgLaunch::Onboarding
        );
    }

    #[test]
    fn a_pipe_is_always_stdio_even_with_our_bundle_id() {
        // MCP client transport = pipe. Never hijack it, whatever the env says.
        assert_eq!(
            classify_no_arg_launch(Some("tech.fixedwidth.glass"), true),
            NoArgLaunch::StdioServe
        );
    }

    #[test]
    fn foreign_or_absent_bundle_id_is_stdio() {
        assert_eq!(
            classify_no_arg_launch(Some("com.example.client"), false),
            NoArgLaunch::StdioServe
        );
        assert_eq!(classify_no_arg_launch(None, false), NoArgLaunch::StdioServe);
    }
}
