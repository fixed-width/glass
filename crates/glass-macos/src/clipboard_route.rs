//! Pure clipboard-routing policy for the macOS backend. Decides how `glass_clipboard_get/set`
//! behave for the active session, mirroring the Windows `ClipboardRoute`. No OS calls — unit
//! -tested on the Linux dev box.
#![forbid(unsafe_code)]

use glass_core::platform::SandboxLevel;

/// How `MacosPlatform` routes clipboard for the active session. Set at `start_app`, reset on
/// `stop_app`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ClipboardRoute {
    /// `sandbox: off` — the real general system pasteboard.
    #[default]
    RealGeneral,
    /// Contained + injectable + the shim's sentinel was observed — a private named pasteboard
    /// the shim redirected the app to; glass shares it by this name.
    Private(String),
    /// Contained + hardened (injection would be stripped), or injectable-but-injection-
    /// unconfirmed — clipboard is isolated with no bridge (fail-closed).
    Unsupported,
}

/// Decide the route from the launch facts. `injectable` = the target is not hardened-runtime
/// (so DYLD injection can take); `shim_confirmed` = the shim's sentinel was seen on the named
/// pasteboard after launch (injection actually took).
pub fn decide_route(
    level: SandboxLevel,
    name: &str,
    injectable: bool,
    shim_confirmed: bool,
) -> ClipboardRoute {
    if level == SandboxLevel::Off {
        return ClipboardRoute::RealGeneral;
    }
    if injectable && shim_confirmed {
        ClipboardRoute::Private(name.to_owned())
    } else {
        ClipboardRoute::Unsupported
    }
}

/// The per-session named-pasteboard name shared between glass and the shim (glass sets it in
/// the child's `GLASS_CLIP_PASTEBOARD` env var). `token` is a per-spawn unique value (an
/// atomic counter) so concurrent/relaunched sessions never collide.
pub fn session_pasteboard_name(token: u64) -> String {
    format!("tech.fixedwidth.glass.clip.{token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_real_general() {
        assert_eq!(
            decide_route(SandboxLevel::Off, "n", false, false),
            ClipboardRoute::RealGeneral
        );
    }
    #[test]
    fn contained_injectable_confirmed_is_private() {
        assert_eq!(
            decide_route(SandboxLevel::Default, "n", true, true),
            ClipboardRoute::Private("n".into())
        );
    }
    #[test]
    fn contained_hardened_is_unsupported() {
        assert_eq!(
            decide_route(SandboxLevel::Strict, "n", false, false),
            ClipboardRoute::Unsupported
        );
    }
    #[test]
    fn contained_injectable_but_unconfirmed_is_unsupported() {
        assert_eq!(
            decide_route(SandboxLevel::Default, "n", true, false),
            ClipboardRoute::Unsupported
        );
    }
    #[test]
    fn session_name_is_token_scoped() {
        assert_eq!(session_pasteboard_name(7), "tech.fixedwidth.glass.clip.7");
        assert_ne!(session_pasteboard_name(1), session_pasteboard_name(2));
    }
}
