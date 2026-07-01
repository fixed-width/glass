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
    CoordOutOfBounds { x: i32, y: i32, width: u32, height: u32 },

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

    #[error("element #{0} is not editable")]
    AxElementNotEditable(u32),

    #[error("element #{0} changed since the snapshot; re-snapshot")]
    AxElementChanged(u32),

    #[error("set_value on element #{0} reported success but the value did not change (read-only a11y projection — use keystrokes)")]
    AxValueNotApplied(u32),

    #[error("accessibility unavailable: {0}")]
    AccessibilityUnavailable(String),

    /// A sandbox was requested but the mechanism is unavailable on a host that
    /// supports it. Carries an actionable remedy.
    #[error("{0}")]
    SandboxUnavailable(String),

    #[error("{0} is not supported by this backend")]
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

/// Convenience alias used throughout glass-core.
pub type Result<T> = std::result::Result<T, GlassError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_actionable() {
        assert_eq!(GlassError::NoActiveSession.to_string(), "no active session");
        assert_eq!(
            GlassError::CoordOutOfBounds { x: 5, y: 9, width: 4, height: 4 }.to_string(),
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
            "element #5 is not editable"
        );
        assert_eq!(
            GlassError::AxElementChanged(2).to_string(),
            "element #2 changed since the snapshot; re-snapshot"
        );
    }

    #[test]
    fn unsupported_message_is_actionable() {
        assert_eq!(
            GlassError::Unsupported("clipboard".into()).to_string(),
            "clipboard is not supported by this backend"
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
