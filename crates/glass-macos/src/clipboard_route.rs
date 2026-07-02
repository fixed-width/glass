//! Pure clipboard-routing policy for the macOS backend. Decides how `glass_clipboard_get/set`
//! behave for the active session, mirroring the Windows `ClipboardRoute`. No OS calls — unit
//! -tested on the Linux dev box.
#![forbid(unsafe_code)]

use glass_core::platform::SandboxLevel;

/// How `MacosPlatform` routes clipboard for the active session. Set at `start_app`, reset on
/// `stop_app`. The default is the fail-closed `Unsupported` route: until `start_app` proves a
/// safe route, clipboard access is denied rather than silently reaching the real pasteboard.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ClipboardRoute {
    /// `sandbox: off` — the real general system pasteboard.
    RealGeneral,
    /// Contained + injectable + the shim's sentinel was observed — a private named pasteboard
    /// the shim redirected the app to; glass shares it by this name.
    Private(String),
    /// Contained + hardened (injection would be stripped), or injectable-but-injection-
    /// unconfirmed — clipboard is isolated with no bridge (fail-closed). The default route.
    #[default]
    Unsupported,
}

/// Decide the route from the launch facts. `attempt` is `Some((name, shim_confirmed))` for a
/// contained, injectable launch — `name` is the private named pasteboard glass shares with the
/// shim, and `shim_confirmed` is whether the shim's sentinel was observed after launch
/// (injection actually took) — or `None` for an uncontained (`sandbox: off`) or non-injectable
/// launch.
///
/// Fail-closed: only `SandboxLevel::Off` (→ `RealGeneral`) or a confirmed injection
/// (→ `Private`) reaches a real/private pasteboard; every other case is `Unsupported`.
pub fn decide_route(level: SandboxLevel, attempt: Option<(&str, bool)>) -> ClipboardRoute {
    if level == SandboxLevel::Off {
        return ClipboardRoute::RealGeneral;
    }
    match attempt {
        Some((name, true)) => ClipboardRoute::Private(name.to_owned()),
        _ => ClipboardRoute::Unsupported,
    }
}

/// The per-session named-pasteboard name shared between glass and the shim (glass sets it in
/// the child's `GLASS_CLIP_PASTEBOARD` env var). Combines the launching `glass-mcp` process's
/// `pid`, a per-launch `nonce` (wall-clock nanoseconds), and a per-spawn `token` (an atomic
/// counter) so the name is unique per launch. The `pid`/`nonce` components are what keep it
/// unique ACROSS `glass-mcp` restarts: a bare counter resets to its start value on every
/// restart, so a reused name would let a stale, system-wide named pasteboard (they persist
/// until released) mask a failed injection with an old sentinel.
pub fn session_pasteboard_name(pid: u32, nonce: u64, token: u64) -> String {
    format!("tech.fixedwidth.glass.clip.{pid}.{nonce}.{token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_real_general() {
        assert_eq!(decide_route(SandboxLevel::Off, None), ClipboardRoute::RealGeneral);
    }
    #[test]
    fn off_is_real_general_even_with_a_confirmed_attempt() {
        // `Off` short-circuits before the attempt is considered — an uncontained launch never
        // carries one, but the precedence must be explicit.
        assert_eq!(
            decide_route(SandboxLevel::Off, Some(("n", true))),
            ClipboardRoute::RealGeneral
        );
    }
    #[test]
    fn contained_injectable_confirmed_is_private() {
        assert_eq!(
            decide_route(SandboxLevel::Default, Some(("n", true))),
            ClipboardRoute::Private("n".into())
        );
    }
    #[test]
    fn contained_with_no_attempt_is_unsupported() {
        assert_eq!(decide_route(SandboxLevel::Strict, None), ClipboardRoute::Unsupported);
    }
    #[test]
    fn contained_injectable_but_unconfirmed_is_unsupported() {
        assert_eq!(
            decide_route(SandboxLevel::Default, Some(("n", false))),
            ClipboardRoute::Unsupported
        );
    }
    #[test]
    fn default_route_is_fail_closed_unsupported() {
        assert_eq!(ClipboardRoute::default(), ClipboardRoute::Unsupported);
    }
    #[test]
    fn session_name_includes_pid_nonce_and_token() {
        assert_eq!(session_pasteboard_name(42, 7, 3), "tech.fixedwidth.glass.clip.42.7.3");
    }
    #[test]
    fn session_name_differs_when_any_input_differs() {
        let base = session_pasteboard_name(1, 1, 1);
        assert_ne!(base, session_pasteboard_name(2, 1, 1), "pid differs");
        assert_ne!(base, session_pasteboard_name(1, 2, 1), "nonce differs");
        assert_ne!(base, session_pasteboard_name(1, 1, 2), "token differs");
    }
}
