use thiserror::Error;

/// All fallible glass-core operations return this error.
///
/// Variants map to the actionable error kinds the MCP layer surfaces to the
/// agent. Backend crates fold their OS-specific failures into `Backend`.
#[derive(Debug, Error)]
pub enum GlassError {
    #[error("no active session")]
    NoActiveSession,

    #[error("app failed to start: {0}")]
    AppNotStarted(String),

    #[error("app exited (code {0:?})")]
    AppExited(Option<i32>),

    #[error("window not found")]
    WindowNotFound,

    #[error("capture failed: {0}")]
    CaptureFailed(String),

    #[error("baseline not found: {0}")]
    BaselineMissing(String),

    #[error("operation timed out after {0} ms")]
    Timeout(u64),

    #[error("coordinate ({x},{y}) out of bounds for {width}x{height} window")]
    CoordOutOfBounds {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },

    #[error("invalid key: {0}")]
    InvalidKey(String),

    #[error("invalid name: {0}")]
    InvalidName(String),

    #[error("invalid region: {0}")]
    InvalidRegion(String),

    #[error("frames differ in size: {a:?} vs {b:?}")]
    SizeMismatch { a: (u32, u32), b: (u32, u32) },

    #[error("image codec error: {0}")]
    ImageCodec(String),

    #[error("accessibility is not supported by this backend")]
    AxUnsupported,

    #[error("no accessibility snapshot yet; call glass_a11y_snapshot first")]
    NoAxSnapshot,

    #[error("element #{0} is not in the current snapshot; re-snapshot")]
    AxElementNotFound(u32),

    #[error("element #{0} has no clickable on-screen geometry")]
    AxElementNotClickable(u32),

    #[error("element #{0} is not editable via the accessibility API (its a11y projection exposes no writable value — a common toolkit gap even when the element accepts typed input); focus it with glass_click, then enter text with glass_type / glass_key instead")]
    AxElementNotEditable(u32),

    #[error("element #{0} has no option matching {1:?}; available options: {2}")]
    AxOptionNotFound(u32, String, String),

    #[error("element #{0} changed since the snapshot; re-snapshot")]
    AxElementChanged(u32),

    #[error("set_value on element #{0} reported success but the value did not change (read-only a11y projection — use keystrokes)")]
    AxValueNotApplied(u32),

    #[error("element #{0} is inside a popover glass could not map to a window; select_window it and click by coordinate")]
    AxElementInUnmappedPopover(u32),

    #[error("accessibility unavailable: {0}")]
    AccessibilityUnavailable(String),

    /// A sandbox was requested but the mechanism is unavailable on a host that
    /// supports it. Carries an actionable remedy.
    #[error("{0}")]
    SandboxUnavailable(String),

    #[error("{0}")]
    Unsupported(String),

    /// A required OS permission is not granted. Carries which permission and how to
    /// grant it, so the MCP layer can tell the agent exactly what to do. Never paper
    /// over this with a blank frame (no-silent-fallback invariant).
    #[error("{which} permission denied: {remedy}")]
    PermissionDenied { which: String, remedy: String },

    #[error("backend error: {0}")]
    Backend(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl GlassError {
    /// Runtime "this operation is unsupported on the active backend" error, worded
    /// consistently.
    ///
    /// Callers pass the capability's own key and `note` (read from that backend's
    /// [`crate::CapabilityMap`]), so the message stays in sync with that backend's
    /// capability map without this constructor reaching into it itself. `operation` is
    /// the [`crate::CapabilityMap`] field key (e.g. `"multi_touch"`); the message embeds
    /// it verbatim, so it is the exact key `glass_capabilities` lists — the agent can
    /// cross-reference the two. `backend` is the **active** backend's name. `note` is
    /// folded in when present. Always points the agent at `glass_capabilities`.
    pub fn unsupported(operation: &str, backend: &str, note: Option<&str>) -> Self {
        use std::fmt::Write as _;
        let mut msg = format!("{operation} is not supported by the {backend} backend");
        if let Some(n) = note {
            let _ = write!(msg, " ({n})");
        }
        msg.push_str("; call glass_capabilities to see what this backend can do");
        GlassError::Unsupported(msg)
    }
}

/// Convenience alias used throughout glass-core.
pub type Result<T> = std::result::Result<T, GlassError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_actionable() {
        assert_eq!(GlassError::NoActiveSession.to_string(), "no active session");
        assert_eq!(
            GlassError::CoordOutOfBounds {
                x: 5,
                y: 9,
                width: 4,
                height: 4
            }
            .to_string(),
            "coordinate (5,9) out of bounds for 4x4 window"
        );
        assert_eq!(
            GlassError::BaselineMissing("main".into()).to_string(),
            "baseline not found: main"
        );
    }

    #[test]
    fn io_errors_convert() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let err: GlassError = io.into();
        assert!(matches!(err, GlassError::Io(_)));
    }

    #[test]
    fn a11y_messages_are_actionable() {
        assert_eq!(
            GlassError::AxUnsupported.to_string(),
            "accessibility is not supported by this backend"
        );
        assert_eq!(
            GlassError::NoAxSnapshot.to_string(),
            "no accessibility snapshot yet; call glass_a11y_snapshot first"
        );
        assert_eq!(
            GlassError::AxElementNotFound(7).to_string(),
            "element #7 is not in the current snapshot; re-snapshot"
        );
        assert_eq!(
            GlassError::AxElementNotClickable(3).to_string(),
            "element #3 has no clickable on-screen geometry"
        );
        assert_eq!(
            GlassError::AxElementNotEditable(5).to_string(),
            "element #5 is not editable via the accessibility API (its a11y projection exposes no writable value — a common toolkit gap even when the element accepts typed input); focus it with glass_click, then enter text with glass_type / glass_key instead"
        );
        assert_eq!(
            GlassError::AxElementChanged(2).to_string(),
            "element #2 changed since the snapshot; re-snapshot"
        );
        assert_eq!(
            GlassError::AxElementInUnmappedPopover(9).to_string(),
            "element #9 is inside a popover glass could not map to a window; select_window it and click by coordinate"
        );
    }

    #[test]
    fn unsupported_message_is_actionable() {
        // Default trait impls that cannot know the active backend keep the generic phrase.
        assert_eq!(
            GlassError::Unsupported("clipboard is not supported by this backend".into())
                .to_string(),
            "clipboard is not supported by this backend"
        );
    }

    #[test]
    fn unsupported_display_is_the_raw_payload() {
        assert_eq!(
            GlassError::Unsupported("anything at all".into()).to_string(),
            "anything at all"
        );
    }

    #[test]
    fn unsupported_constructor_names_backend_and_points_at_capabilities() {
        let e = GlassError::unsupported("multi_touch", "x11", None);
        assert_eq!(
            e.to_string(),
            "multi_touch is not supported by the x11 backend; \
             call glass_capabilities to see what this backend can do"
        );
    }

    #[test]
    fn unsupported_constructor_folds_in_the_note_when_present() {
        let e = GlassError::unsupported(
            "window_move_resize",
            "android",
            Some("apps are full-screen"),
        );
        assert_eq!(
            e.to_string(),
            "window_move_resize is not supported by the android backend (apps are full-screen); \
             call glass_capabilities to see what this backend can do"
        );
    }

    #[test]
    fn accessibility_unavailable_message_is_actionable() {
        assert_eq!(
            GlassError::AccessibilityUnavailable("no a11y bus".into()).to_string(),
            "accessibility unavailable: no a11y bus"
        );
    }

    #[test]
    fn permission_denied_renders_which_and_remedy() {
        let e = GlassError::PermissionDenied {
            which: "Screen Recording".into(),
            remedy: "grant GlassProbe in System Settings > Privacy & Security".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("Screen Recording"), "{msg}");
        assert!(msg.contains("System Settings"), "{msg}");
    }
}
