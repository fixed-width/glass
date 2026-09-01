use thiserror::Error;

use crate::Whose;

/// Which bound ended a call that produced no answer — the distinction
/// [`crate::run_bounded_until`] makes and [`GlassError::Bounded`] carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundKind {
    /// The call ran and was killed when its effective bound elapsed — its own budget or the
    /// deadline it shares, whichever was nearer.
    ///
    /// [`GlassError::bound_owner`] says which of the two governed: callee-owned expiry indicates the
    /// backend exceeded its own ceiling; caller-owned expiry says nothing about backend health. Use
    /// [`GlassError::bound_dispatch`] separately for possible external effects (glass#341,
    /// glass#347).
    TimedOut,
    /// The call never ran: the deadline it shares with the rest of a sequence was already spent.
    /// Nothing was asked, so nothing about the tool is known.
    NotStarted,
}

/// Whether a bounded failure proves that no external work was dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundDispatch {
    /// The bound was spent before glass dispatched any external work.
    NotDispatched,
    /// External work started, so its effect may have occurred before the bound ended the wait.
    MayHaveDispatched,
}

/// Render a backend's own explanation as the clause closing [`GlassError::AxValueNotApplied`].
fn render_why(why: &Option<&'static str>) -> String {
    why.map(|w| format!(" — {w}")).unwrap_or_default()
}

/// Render a read-back for [`GlassError::AxValueNotApplied`] as the clause following "the element".
///
/// `None` is not an empty element — that reads back as `Some("")` — it is no reading at all: the
/// platform's read failed, or nothing matching the element was found.
fn render_observed(observed: &Option<String>) -> String {
    match observed {
        Some(v) => format!("holds {v:?}"),
        None => "could not be read back".to_string(),
    }
}

/// All fallible glass-core operations return this error.
///
/// Variants map to the actionable error kinds the MCP layer surfaces to the
/// agent. Backend crates fold their OS-specific failures into `Backend`.
#[derive(Debug, Error)]
pub enum GlassError {
    #[error("no active session — call glass_start to launch an app first")]
    NoActiveSession,

    #[error("cannot change protected host paths while a session is active")]
    ProtectedPathsWhileActive,

    #[error("app failed to start: {0}")]
    AppNotStarted(String),

    #[error("app exited (code {0:?})")]
    AppExited(Option<i32>),

    /// Same failure as `AppExited`, but the launch was sandboxed — so the
    /// exit may be the contained app failing to find a file the ephemeral
    /// tmpfs hides. Carries an actionable remedy rather than internal sandbox
    /// mechanics.
    #[error(
        "app exited (code {0:?}) before its window appeared. The launch was sandboxed, so a file \
         it needs may be hidden by the ephemeral $HOME or /tmp — set `cwd`, or run with \
         sandbox:\"off\"."
    )]
    SandboxedAppExited(Option<i32>),

    /// No window matched. Two causes reach here and the error cannot tell them apart: the app
    /// may not have opened its window yet, or the window glass is targeting is no longer one of
    /// its windows — every backend's resolver hits the same ambiguous `Option`/lookup miss
    /// (#263). On macOS the discriminating per-candidate detail is printed to stderr by the
    /// resolver that failed; other backends have no equivalent diagnostic yet.
    #[error(
        "window not found — the app may not have opened its window yet, or the window glass is \
         targeting is no longer one of its windows"
    )]
    WindowNotFound,

    /// A launch was accepted but produced no window glass can drive before the deadline, carrying
    /// what the backend saw on screen instead. Distinct from [`Self::AppNotStarted`], which means
    /// the launch itself failed: the app here may be running, with another app's window in front
    /// of it — which a bare [`Self::Timeout`] cannot say (glass#338).
    #[error("no window for {package} appeared within {timeout_ms} ms — {observed}")]
    AppWindowNotVisible {
        package: String,
        timeout_ms: u64,
        observed: String,
    },

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

    #[error("invalid pointer input: {0}")]
    InvalidPointerInput(&'static str),

    #[error("frames differ in size: {a:?} vs {b:?}")]
    SizeMismatch { a: (u32, u32), b: (u32, u32) },

    #[error("image codec error: {0}")]
    ImageCodec(String),

    #[error(
        "accessibility is not supported by this backend — see the app with glass_screenshot \
         and drive it by pixel coordinates (glass_click) instead"
    )]
    AxUnsupported,

    #[error("no accessibility snapshot yet; call glass_a11y_snapshot first")]
    NoAxSnapshot,

    #[error("element #{0} is not in the current snapshot; re-snapshot")]
    AxElementNotFound(u32),

    #[error(
        "element #{0} has no clickable on-screen geometry — it's off-screen or its a11y node \
         reports no bounds; bring it into view with glass_scroll_to_element (re-snapshots), then \
         retry, or locate it with glass_screenshot and click by coordinate"
    )]
    AxElementNotClickable(u32),

    #[error(
        "element #{0} is not editable via the accessibility API (its a11y projection exposes no writable value — a common toolkit gap even when the element accepts typed input); focus it with glass_click, then enter text with glass_type / glass_key instead"
    )]
    AxElementNotEditable(u32),

    #[error("element #{0} has no option matching {1:?}; available options: {2}")]
    AxOptionNotFound(u32, String, String),

    #[error("element #{0} changed since the snapshot; re-snapshot")]
    AxElementChanged(u32),

    /// Raised in place of [`Self::AxElementChanged`] when nothing in the tree presents as the
    /// element any more.
    #[error(
        "element #{0} is gone — nothing in the tree carries its role and name any more, so it has \
         not moved or been renumbered; the screen it was on was replaced, or the app that drew it \
         restarted. Re-snapshot to see where the app is now rather than re-addressing this element"
    )]
    AxElementGone(u32),

    /// A write that went out and could not then be confirmed. Distinct from the pre-write
    /// refusals because only this one has dispatched — see `set_value_failed_after_writing`.
    #[error(
        "element #{0}: the write went out, but could not be confirmed — {1}. Re-snapshot to see \
         where it landed rather than writing it again"
    )]
    AxWriteUnconfirmed(u32, String),

    /// A write that went out and could not be confirmed because another structured operation
    /// failed. The source remains available for timeout ownership and transport classification;
    /// the outer verdict remains authoritative for retry safety and dispatch provenance.
    #[error(
        "element #{id}: the write went out, but could not be confirmed — {detail} ({source}). \
         Re-snapshot to see where it landed rather than writing it again"
    )]
    AxWriteUnconfirmedCaused {
        id: u32,
        detail: String,
        #[source]
        source: Box<GlassError>,
    },

    /// A dispatched write whose read-back does not hold the request.
    ///
    /// Carries both values because three outcomes look alike from the id alone: the element
    /// transformed the write and holds it in another form (writing again changes nothing), it holds
    /// part of the request (a keystroke was dropped, so writing again is the fix), or it holds what
    /// it held before (the write took no effect). Build it with [`GlassError::value_not_applied`],
    /// or [`GlassError::value_not_applied_because`] for the last case, which is the only one a
    /// backend's own explanation fits — [`crate::write_took_no_effect`] is the test for it.
    #[error(
        "set_value on element #{id} did not take — asked for {requested:?}, the element {}. \
         Holding the request in another form means the element transformed it, and writing again \
         will not change that; holding part of it, or none of it, means the write did not take \
         effect{}",
        render_observed(.observed),
        render_why(.why)
    )]
    AxValueNotApplied {
        id: u32,
        requested: String,
        /// What the element reads as now: the text for a field — `Some("")` when it is empty — or
        /// `"on"` / `"off"` for a boolean control. `None` only when no reading was obtained.
        observed: Option<String>,
        /// What this element's own backend knows about a write that does not arrive, which the
        /// shared message cannot say: it is written for no backend in particular, and a remedy
        /// aimed at the wrong one sends a caller somewhere futile.
        why: Option<&'static str>,
    },

    #[error("element #{0} exposes no native activation action")]
    AxActionUnavailable(u32),

    #[error("native action on element #{0} failed: {1}")]
    AxActionFailed(u32, String),

    #[error(
        "set_value on element #{0} is a switch/checkbox and expects a boolean — one of true/false, on/off, 1/0, yes/no (got {1:?})"
    )]
    AxValueNotBoolean(u32, String),

    #[error(
        "element #{0} is inside a popover glass could not map to a window; select_window it and click by coordinate"
    )]
    AxElementInUnmappedPopover(u32),

    #[error("accessibility unavailable: {0}")]
    AccessibilityUnavailable(String),

    /// The app is up but has published no accessibility tree *yet*. Distinct from
    /// [`Self::AccessibilityUnavailable`] because waiting can resolve it and nothing else can:
    /// a wait polls through this instead of abandoning its budget on the first read (glass#329),
    /// where a session launched without `a11y: true` is wrong however long anyone waits.
    ///
    /// An app that never publishes one is indistinguishable from a slow one at any single read,
    /// so this is what a wait reports when its whole budget goes by.
    #[error("accessibility not ready: {0}")]
    AccessibilityNotReady(String),

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

    /// A backend failure the backend itself reported. [`Self::Bounded`] and [`Self::ToolFailed`]
    /// display identically and this variant is false for both, so classify with [`Self::bound`]
    /// and [`Self::tool_said`] rather than by matching it.
    #[error("backend error: {0}")]
    Backend(String),

    /// A tool glass drove ran and exited non-zero, carrying what it wrote to stderr.
    ///
    /// `said` is that stderr, trimmed — empty when the tool failed without a word, which for
    /// `uiautomator` is a crash whose trace went to the platform log instead (glass#341) and
    /// waiting resolves. Read it through [`Self::tool_said`], never out of the rendered message,
    /// which the tool's own output can imitate (glass#348). A tool that explains itself on
    /// *stdout* reads as silent here.
    ///
    /// Not `#[non_exhaustive]`, unlike its sibling [`Self::Bounded`]: every backend that drives an
    /// external tool raises this legitimately, where only glass's own clock raises a bound.
    #[error("backend error: `{call}` failed: {said}")]
    ToolFailed { call: String, said: String },

    /// A bounded call that ended at one of its bounds instead of at an answer, naming which.
    ///
    /// Displays exactly as [`Self::Backend`] — an agent reads the same text — but is a distinct
    /// variant, so glass can tell its own deadline firing from the tool failing without reading
    /// that back out of the message, which carries the child's own output verbatim (glass#348).
    ///
    /// `#[non_exhaustive]` so [`crate::bounded`] stays the only crate that can raise one: a bound
    /// forged elsewhere — an iOS gRPC deadline, say — would make [`Self::bound`] answer for a
    /// clock glass does not own.
    #[non_exhaustive]
    #[error("backend error: {message}")]
    Bounded {
        kind: BoundKind,
        whose: Whose,
        dispatch: BoundDispatch,
        message: String,
    },

    /// Both the pointer operation and restoration failed; each structured cause remains
    /// inspectable.
    #[error("{primary}; restoring the previous active window failed: {restore}")]
    WindowRestoreFailed {
        #[source]
        primary: Box<GlassError>,
        restore: Box<GlassError>,
    },

    /// Both input and mandatory release failed; either cause may own the deadline, and input state
    /// is uncertain.
    #[error("{primary}; cleanup failed while {operation}: {cleanup}")]
    InputCleanupFailed {
        operation: &'static str,
        #[source]
        primary: Box<GlassError>,
        cleanup: Box<GlassError>,
    },

    /// An unchanged failure from work proven not to have dispatched.
    #[error(transparent)]
    BeforeDispatch(Box<GlassError>),

    /// An unchanged failure following earlier dispatch in the same compound operation.
    #[error(transparent)]
    AfterDispatch(Box<GlassError>),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl GlassError {
    /// A sequence step rejected before dispatch because its shared deadline was already spent.
    pub fn deadline_not_started(op: &str) -> Self {
        GlassError::Bounded {
            kind: BoundKind::NotStarted,
            whose: Whose::Caller,
            dispatch: BoundDispatch::NotDispatched,
            message: format!(
                "{op}: the deadline it shares with the rest of the call was already spent, so it was not started"
            ),
        }
    }

    /// A bounded operation started, then the caller's shared deadline elapsed before it answered.
    pub fn caller_deadline_elapsed(op: &str) -> Self {
        Self::caller_deadline_elapsed_with_guidance(op, "")
    }

    /// A caller-owned timeout for dispatched work whose answer or effect may still arrive.
    pub fn caller_deadline_elapsed_with_guidance(op: &str, guidance: &str) -> Self {
        let guidance = if guidance.is_empty() {
            String::new()
        } else {
            format!("; {guidance}")
        };
        GlassError::Bounded {
            kind: BoundKind::TimedOut,
            whose: Whose::Caller,
            dispatch: BoundDispatch::MayHaveDispatched,
            message: format!(
                "{op}: the caller deadline elapsed before the operation answered{guidance}"
            ),
        }
    }

    /// Whether native invoke may fall back to a pointer click.
    ///
    /// Only [`GlassError::AxUnsupported`] and [`GlassError::AxActionUnavailable`] prove no dispatch;
    /// every other failure propagates to prevent double actuation.
    pub fn invoke_fallback_eligible(&self) -> bool {
        matches!(
            self,
            GlassError::AxUnsupported | GlassError::AxActionUnavailable(_)
        )
    }

    /// The underlying structured failure, recursively unwrapping dispatch annotations.
    pub fn cause(&self) -> &Self {
        match self {
            GlassError::BeforeDispatch(error) | GlassError::AfterDispatch(error) => error.cause(),
            GlassError::AxWriteUnconfirmedCaused { source, .. } => source.cause(),
            GlassError::WindowRestoreFailed { primary, .. } => primary.cause(),
            GlassError::InputCleanupFailed { primary, .. } => primary.cause(),
            error => error,
        }
    }

    /// Whether the value mutation itself may have dispatched, requiring its cached value to be
    /// dropped.
    ///
    /// Backends classify ambiguous transport failures where they know the write may have reached
    /// the device.
    pub fn set_value_failed_after_writing(&self) -> bool {
        match self {
            GlassError::AxValueNotApplied { .. }
            | GlassError::AxWriteUnconfirmed(..)
            | GlassError::AxWriteUnconfirmedCaused { .. } => true,
            GlassError::BeforeDispatch(error) | GlassError::AfterDispatch(error) => {
                error.set_value_failed_after_writing()
            }
            GlassError::WindowRestoreFailed { primary, restore } => {
                primary.set_value_failed_after_writing() || restore.set_value_failed_after_writing()
            }
            GlassError::InputCleanupFailed {
                primary, cleanup, ..
            } => {
                primary.set_value_failed_after_writing() || cleanup.set_value_failed_after_writing()
            }
            _ => false,
        }
    }

    /// Preserve proof that this ordinary failure occurred before dispatch; stronger existing
    /// provenance remains unchanged.
    pub fn before_dispatch(self) -> Self {
        match self {
            error @ (GlassError::BeforeDispatch(_)
            | GlassError::AfterDispatch(_)
            | GlassError::Bounded { .. }
            | GlassError::InputCleanupFailed { .. }) => error,
            error => GlassError::BeforeDispatch(Box::new(error)),
        }
    }

    /// Preserve earlier dispatch when a later compound-operation step fails.
    pub fn after_dispatch(self) -> Self {
        match self {
            error @ (GlassError::AfterDispatch(_) | GlassError::InputCleanupFailed { .. }) => error,
            error @ GlassError::Bounded {
                dispatch: BoundDispatch::MayHaveDispatched,
                ..
            } => error,
            error => GlassError::AfterDispatch(Box::new(error)),
        }
    }

    /// Attach the structured cause that prevented confirmation unless a stronger post-write verdict
    /// already exists.
    pub fn write_unconfirmed_because(
        id: u32,
        detail: impl Into<String>,
        source: GlassError,
    ) -> GlassError {
        if source.set_value_failed_after_writing() {
            source
        } else {
            GlassError::AxWriteUnconfirmedCaused {
                id,
                detail: detail.into(),
                source: Box::new(source),
            }
        }
    }

    /// The verdict for a write that dispatched and whose read-back does not hold the request.
    ///
    /// Pass the value the verification already read: one taken afterwards can catch a value that
    /// arrived late and contradict the verdict it explains. A backend whose mapper drops an empty
    /// value must pass `Some("")` rather than `None`, which says no reading was obtained.
    pub fn value_not_applied(id: u32, requested: &str, observed: Option<&str>) -> GlassError {
        GlassError::AxValueNotApplied {
            id,
            requested: requested.to_string(),
            observed: observed.map(str::to_string),
            why: None,
        }
    }

    /// As [`Self::value_not_applied`], plus what this backend knows about a write of its own that
    /// takes no effect — the mechanism, and a remedy where there is one, appended to the shared
    /// message.
    ///
    /// Only for a read-back [`crate::write_took_no_effect`] accepts: the clause subordinates to
    /// that outcome, so attaching it to a transformed value contradicts the sentence before it.
    /// It is appended after `" — "`, so it reads as a continuation — lowercase, no closing stop.
    ///
    /// `&'static str` deliberately: a remedy is a fixed explanation of how this backend writes, not
    /// a place to format the call's data into.
    pub fn value_not_applied_because(
        id: u32,
        requested: &str,
        observed: Option<&str>,
        why: &'static str,
    ) -> GlassError {
        GlassError::AxValueNotApplied {
            id,
            requested: requested.to_string(),
            observed: observed.map(str::to_string),
            why: Some(why),
        }
    }

    /// Preserve both a failed input operation and the mandatory release that also failed.
    pub fn input_cleanup_failed(
        operation: &'static str,
        primary: GlassError,
        cleanup: GlassError,
    ) -> GlassError {
        GlassError::InputCleanupFailed {
            operation,
            primary: Box::new(primary),
            cleanup: Box::new(cleanup),
        }
    }

    /// Which of glass's own bounds ended this call, if one did rather than the tool answering.
    ///
    /// The question a backend asks before retrying, before offering a wedged-tool remedy, and
    /// before reporting a caller's spent budget as a device failure. `None` for every other
    /// failure — including ones a bounded call raises for a tool that did answer, and any variant
    /// added later, which reads as the tool having failed rather than as a bound of glass's.
    pub fn bound(&self) -> Option<BoundKind> {
        match self {
            GlassError::Bounded { kind, .. } => Some(*kind),
            GlassError::BeforeDispatch(error)
            | GlassError::AfterDispatch(error)
            | GlassError::AxWriteUnconfirmedCaused { source: error, .. } => error.bound(),
            GlassError::WindowRestoreFailed { primary, restore } => {
                primary.bound().or_else(|| restore.bound())
            }
            GlassError::InputCleanupFailed {
                primary, cleanup, ..
            } => primary.bound().or_else(|| cleanup.bound()),
            _ => None,
        }
    }

    /// Whose bound ended this call, when [`Self::bound`] reports one.
    pub fn bound_owner(&self) -> Option<Whose> {
        match self {
            GlassError::Bounded { whose, .. } => Some(*whose),
            GlassError::BeforeDispatch(error)
            | GlassError::AfterDispatch(error)
            | GlassError::AxWriteUnconfirmedCaused { source: error, .. } => error.bound_owner(),
            GlassError::WindowRestoreFailed { primary, restore } => {
                primary.bound_owner().or_else(|| restore.bound_owner())
            }
            GlassError::InputCleanupFailed {
                primary, cleanup, ..
            } => primary.bound_owner().or_else(|| cleanup.bound_owner()),
            _ => None,
        }
    }

    /// Whether external work may have been dispatched before this failure.
    pub fn bound_dispatch(&self) -> Option<BoundDispatch> {
        match self {
            GlassError::Bounded { dispatch, .. } => Some(*dispatch),
            GlassError::BeforeDispatch(error) => {
                if error.bound_dispatch() == Some(BoundDispatch::MayHaveDispatched) {
                    Some(BoundDispatch::MayHaveDispatched)
                } else {
                    Some(BoundDispatch::NotDispatched)
                }
            }
            GlassError::AfterDispatch(_)
            | GlassError::AxWriteUnconfirmed(..)
            | GlassError::AxWriteUnconfirmedCaused { .. } => Some(BoundDispatch::MayHaveDispatched),
            GlassError::WindowRestoreFailed { primary, restore } => {
                match (primary.bound_dispatch(), restore.bound_dispatch()) {
                    (Some(BoundDispatch::MayHaveDispatched), _)
                    | (_, Some(BoundDispatch::MayHaveDispatched)) => {
                        Some(BoundDispatch::MayHaveDispatched)
                    }
                    (Some(BoundDispatch::NotDispatched), _)
                    | (_, Some(BoundDispatch::NotDispatched)) => Some(BoundDispatch::NotDispatched),
                    (None, None) => None,
                }
            }
            GlassError::InputCleanupFailed { .. } => Some(BoundDispatch::MayHaveDispatched),
            _ => None,
        }
    }

    /// What a tool wrote to stderr before exiting non-zero, trimmed, if a tool ran at all.
    ///
    /// `Some("")` is the one a backend acts on: a tool that failed saying nothing crashed, and
    /// crashes are worth retrying where a refusal it explained is not. `None` for every other
    /// failure, including a bound firing, and for any variant added later.
    ///
    /// Trimmed here and not only at construction, so a producer that forgets cannot turn a crash
    /// into a refusal.
    pub fn tool_said(&self) -> Option<&str> {
        match self {
            GlassError::ToolFailed { said, .. } => Some(said.trim()),
            GlassError::BeforeDispatch(error)
            | GlassError::AfterDispatch(error)
            | GlassError::AxWriteUnconfirmedCaused { source: error, .. } => error.tool_said(),
            GlassError::WindowRestoreFailed { primary, restore } => {
                primary.tool_said().or_else(|| restore.tool_said())
            }
            GlassError::InputCleanupFailed {
                primary, cleanup, ..
            } => primary.tool_said().or_else(|| cleanup.tool_said()),
            _ => None,
        }
    }

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

    /// The error for "the child exited before `discover_window` found its
    /// window" — `SandboxedAppExited` (with the path-visibility remedy) when
    /// the launch was contained, plain `AppExited` otherwise. Shared by the
    /// Linux backends' discovery loops so the conditional isn't triplicated.
    ///
    /// Takes the launch's [`SandboxLevel`] (not a pre-computed bool) so the
    /// "was this contained?" decision lives here, in one place.
    pub fn app_exited_during_discovery(code: Option<i32>, sandbox: crate::SandboxLevel) -> Self {
        if sandbox != crate::SandboxLevel::Off {
            GlassError::SandboxedAppExited(code)
        } else {
            GlassError::AppExited(code)
        }
    }
}

/// Convenience alias used throughout glass-core.
pub type Result<T> = std::result::Result<T, GlassError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoundDispatch, Whose};

    #[test]
    fn caller_deadline_errors_preserve_the_caller_owner() {
        let error = GlassError::caller_deadline_elapsed("capture");
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
    }

    #[test]
    fn ordinary_backend_errors_have_no_bound_owner() {
        assert_eq!(GlassError::Backend("down".into()).bound_owner(), None);
    }

    #[test]
    fn caller_deadline_errors_preserve_dispatch_uncertainty() {
        let error = GlassError::caller_deadline_elapsed("capture");
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn not_started_deadline_errors_preserve_that_nothing_dispatched() {
        let error = GlassError::deadline_not_started("capture");
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn ordinary_backend_errors_have_no_bound_dispatch() {
        assert_eq!(GlassError::Backend("down".into()).bound_dispatch(), None);
    }

    #[test]
    fn input_cleanup_failure_preserves_both_sources_and_metadata() {
        let error = GlassError::InputCleanupFailed {
            operation: "releasing held input",
            primary: Box::new(GlassError::caller_deadline_elapsed("pointer input")),
            cleanup: Box::new(GlassError::ToolFailed {
                call: "release-input".into(),
                said: "  device refused release  ".into(),
            }),
        };

        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(error.tool_said(), Some("device refused release"));
        assert!(matches!(error.cause(), GlassError::Bounded { .. }));
        let GlassError::InputCleanupFailed {
            operation,
            primary,
            cleanup,
        } = error
        else {
            panic!("cleanup failure must remain structurally inspectable");
        };
        assert_eq!(operation, "releasing held input");
        assert!(matches!(*primary, GlassError::Bounded { .. }));
        assert!(matches!(*cleanup, GlassError::ToolFailed { .. }));
    }

    #[test]
    fn before_dispatch_preserves_an_ordinary_cause_and_marks_no_dispatch() {
        let error = GlassError::Backend("could not spawn helper".into()).before_dispatch();

        assert_eq!(error.to_string(), "backend error: could not spawn helper");
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert!(
            matches!(error.cause(), GlassError::Backend(message) if message == "could not spawn helper")
        );
    }

    #[test]
    fn before_dispatch_accessors_recurse_through_nested_annotations() {
        let bounded = GlassError::BeforeDispatch(Box::new(GlassError::BeforeDispatch(Box::new(
            GlassError::Bounded {
                kind: BoundKind::TimedOut,
                whose: Whose::Callee,
                dispatch: BoundDispatch::MayHaveDispatched,
                message: "nested timeout".into(),
            },
        ))));
        assert_eq!(bounded.bound(), Some(BoundKind::TimedOut));
        assert_eq!(bounded.bound_owner(), Some(Whose::Callee));
        assert_eq!(
            bounded.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched),
            "an inner dispatched bound must outrank outer preflight annotations"
        );
        assert!(matches!(bounded.cause(), GlassError::Bounded { .. }));

        let tool = GlassError::BeforeDispatch(Box::new(GlassError::BeforeDispatch(Box::new(
            GlassError::ToolFailed {
                call: "helper".into(),
                said: " refused \n".into(),
            },
        ))));
        assert_eq!(tool.tool_said(), Some("refused"));
        assert!(matches!(tool.cause(), GlassError::ToolFailed { .. }));
    }

    #[test]
    fn after_dispatch_upgrades_a_before_dispatch_failure_without_marking_a_value_write() {
        let error = GlassError::Backend("later spawn failed".into())
            .before_dispatch()
            .after_dispatch();

        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(
            matches!(error.cause(), GlassError::Backend(message) if message == "later spawn failed")
        );
        assert!(!error.set_value_failed_after_writing());
    }

    #[test]
    fn after_dispatch_preserves_a_coordinate_message_and_marks_prior_dispatch() {
        let error = GlassError::CoordOutOfBounds {
            x: 50,
            y: 50,
            width: 50,
            height: 50,
        }
        .after_dispatch();

        assert_eq!(
            error.to_string(),
            "coordinate (50,50) out of bounds for 50x50 window"
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn after_dispatch_preserves_bounded_kind_owner_message_and_tool_detail() {
        let bounded = GlassError::Bounded {
            kind: BoundKind::NotStarted,
            whose: Whose::Callee,
            dispatch: BoundDispatch::NotDispatched,
            message: "the later read was not started".into(),
        }
        .after_dispatch();
        assert_eq!(bounded.bound(), Some(BoundKind::NotStarted));
        assert_eq!(bounded.bound_owner(), Some(Whose::Callee));
        assert_eq!(
            bounded.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(
            bounded.to_string(),
            "backend error: the later read was not started"
        );

        let tool = GlassError::ToolFailed {
            call: "helper".into(),
            said: " refused \n".into(),
        }
        .after_dispatch();
        assert_eq!(tool.tool_said(), Some("refused"));
        assert_eq!(
            tool.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(
            tool.to_string(),
            "backend error: `helper` failed:  refused \n"
        );
    }

    #[test]
    fn after_dispatch_leaves_an_already_dispatched_bound_directly_matchable() {
        let error = GlassError::caller_deadline_elapsed("later read").after_dispatch();

        assert!(matches!(
            error,
            GlassError::Bounded {
                kind: BoundKind::TimedOut,
                whose: Whose::Caller,
                dispatch: BoundDispatch::MayHaveDispatched,
                ..
            }
        ));
    }

    #[test]
    fn after_dispatch_keeps_invoke_fallback_closed() {
        for error in [
            GlassError::AxUnsupported.after_dispatch(),
            GlassError::AxActionUnavailable(7).after_dispatch(),
        ] {
            assert!(!error.invoke_fallback_eligible(), "{error}");
        }
    }

    #[test]
    fn after_dispatch_does_not_turn_generic_failures_into_value_write_verdicts() {
        assert!(
            !GlassError::Backend("pre-write read failed".into())
                .after_dispatch()
                .set_value_failed_after_writing()
        );
        assert!(
            !GlassError::CoordOutOfBounds {
                x: 50,
                y: 50,
                width: 50,
                height: 50,
            }
            .after_dispatch()
            .set_value_failed_after_writing()
        );

        assert!(
            GlassError::AxWriteUnconfirmed(7, "read-back failed".into())
                .after_dispatch()
                .set_value_failed_after_writing()
        );
        assert!(
            GlassError::value_not_applied(7, "requested", Some("observed"))
                .after_dispatch()
                .set_value_failed_after_writing()
        );
    }

    #[test]
    fn after_dispatch_is_idempotent_and_cause_lookup_recurses() {
        let error = GlassError::CoordOutOfBounds {
            x: 50,
            y: 50,
            width: 50,
            height: 50,
        }
        .after_dispatch()
        .after_dispatch();

        let GlassError::AfterDispatch(inner) = &error else {
            panic!("dispatch provenance must be carried structurally: {error:?}");
        };
        assert!(
            !matches!(inner.as_ref(), GlassError::AfterDispatch(_)),
            "repeated annotation must not grow a wrapper chain: {error:?}"
        );
        assert!(matches!(
            error.cause(),
            GlassError::CoordOutOfBounds {
                x: 50,
                y: 50,
                width: 50,
                height: 50,
            }
        ));
    }

    #[test]
    fn dispatch_provenance_accessors_recurse_through_nested_annotations() {
        let bounded = GlassError::AfterDispatch(Box::new(GlassError::AfterDispatch(Box::new(
            GlassError::Bounded {
                kind: BoundKind::NotStarted,
                whose: Whose::Callee,
                dispatch: BoundDispatch::NotDispatched,
                message: "nested refusal".into(),
            },
        ))));
        assert_eq!(bounded.bound(), Some(BoundKind::NotStarted));
        assert_eq!(bounded.bound_owner(), Some(Whose::Callee));
        assert_eq!(
            bounded.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(matches!(bounded.cause(), GlassError::Bounded { .. }));
        assert_eq!(bounded.to_string(), "backend error: nested refusal");

        let tool = GlassError::AfterDispatch(Box::new(GlassError::AfterDispatch(Box::new(
            GlassError::ToolFailed {
                call: "helper".into(),
                said: " refused \n".into(),
            },
        ))));
        assert_eq!(tool.tool_said(), Some("refused"));
        assert!(matches!(tool.cause(), GlassError::ToolFailed { .. }));
    }

    fn window_restore_failed(primary: GlassError, restore: GlassError) -> GlassError {
        GlassError::WindowRestoreFailed {
            primary: Box::new(primary),
            restore: Box::new(restore),
        }
    }

    #[test]
    fn compound_failures_preserve_a_value_write_from_either_branch() {
        let write = || GlassError::value_not_applied(7, "new", Some("old"));
        let plain = || GlassError::Backend("plain failure".into());

        for error in [
            window_restore_failed(write(), plain()),
            window_restore_failed(plain(), write()),
            GlassError::input_cleanup_failed("cleanup", write(), plain()),
            GlassError::input_cleanup_failed("cleanup", plain(), write()),
        ] {
            assert!(error.set_value_failed_after_writing(), "{error:?}");
        }
    }

    #[test]
    fn window_restore_failure_combines_dispatch_and_tool_details() {
        let error = window_restore_failed(
            GlassError::Backend("primary".into()).before_dispatch(),
            GlassError::ToolFailed {
                call: "restore helper".into(),
                said: " restore failed \n".into(),
            }
            .after_dispatch(),
        );

        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(error.tool_said(), Some("restore failed"));
    }

    #[test]
    fn display_messages_are_actionable() {
        assert_eq!(
            GlassError::NoActiveSession.to_string(),
            "no active session — call glass_start to launch an app first"
        );
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
        assert_eq!(
            GlassError::WindowNotFound.to_string(),
            "window not found — the app may not have opened its window yet, or the window glass \
             is targeting is no longer one of its windows"
        );
    }

    #[test]
    fn a_tool_failure_reads_exactly_as_the_backend_error_it_replaced() {
        // The split is a channel for glass, not a message change for the agent — `glass-android`
        // built this string by hand before it was a variant, down to the trailing space that a
        // tool which said nothing leaves.
        assert_eq!(
            GlassError::ToolFailed {
                call: "adb shell cat /sdcard/x.xml".into(),
                said: "cat: /sdcard/x.xml: No such file".into(),
            }
            .to_string(),
            "backend error: `adb shell cat /sdcard/x.xml` failed: cat: /sdcard/x.xml: No such file"
        );
        assert_eq!(
            GlassError::ToolFailed {
                call: "adb shell uiautomator dump /sdcard/x.xml".into(),
                said: String::new(),
            }
            .to_string(),
            "backend error: `adb shell uiautomator dump /sdcard/x.xml` failed: "
        );
    }

    #[test]
    fn only_a_tool_that_ran_and_said_nothing_reads_as_a_crash() {
        // `Some("")` and `None` are the two a backend must not confuse: one is a tool that failed
        // without a word, which waiting can resolve; the other has no stderr to speak for it at
        // all, a killed call included — [`BoundKind::TimedOut`] ran the tool.
        assert_eq!(
            GlassError::ToolFailed {
                call: "adb shell uiautomator dump /sdcard/x.xml".into(),
                said: String::new(),
            }
            .tool_said(),
            Some("")
        );
        assert_eq!(
            GlassError::ToolFailed {
                call: "adb shell cat /sdcard/x.xml".into(),
                said: "No such file".into(),
            }
            .tool_said(),
            Some("No such file")
        );
        for e in [
            GlassError::Backend("device offline".into()),
            GlassError::Bounded {
                kind: BoundKind::TimedOut,
                whose: Whose::Callee,
                dispatch: BoundDispatch::MayHaveDispatched,
                message: "adb:shell: no answer within 10s".into(),
            },
            GlassError::AccessibilityUnavailable("uiautomator dump wrote nothing".into()),
            GlassError::AccessibilityNotReady("no tree yet".into()),
        ] {
            assert_eq!(e.tool_said(), None, "{e}");
        }
    }

    #[test]
    fn a_tool_that_wrote_only_whitespace_still_reads_as_having_said_nothing() {
        // A crash that manages a bare newline is still a crash — trimmed on the way out as well
        // as on the way in, so removing either leaves the other holding this.
        assert_eq!(
            GlassError::ToolFailed {
                call: "adb shell uiautomator dump /sdcard/x.xml".into(),
                said: "\n  ".into(),
            }
            .tool_said(),
            Some("")
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
            "accessibility is not supported by this backend — see the app with glass_screenshot and drive it by pixel coordinates (glass_click) instead"
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
            "element #3 has no clickable on-screen geometry — it's off-screen or its a11y node reports no bounds; bring it into view with glass_scroll_to_element (re-snapshots), then retry, or locate it with glass_screenshot and click by coordinate"
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
    fn a_gone_element_forecloses_the_drift_hunt_its_neighbour_invites() {
        assert_eq!(
            GlassError::AxElementGone(16).to_string(),
            "element #16 is gone — nothing in the tree carries its role and name any more, so it \
             has not moved or been renumbered; the screen it was on was replaced, or the app that \
             drew it restarted. Re-snapshot to see where the app is now rather than re-addressing \
             this element"
        );
    }

    #[test]
    fn ax_action_errors_name_the_element_and_cause() {
        assert_eq!(
            GlassError::AxActionUnavailable(7).to_string(),
            "element #7 exposes no native activation action"
        );
        assert_eq!(
            GlassError::AxActionFailed(7, "action reported failure".into()).to_string(),
            "native action on element #7 failed: action reported failure"
        );
    }

    #[test]
    fn invoke_fallback_is_eligible_only_when_nothing_was_dispatched() {
        // Eligible: the backend never dispatched an action, so a pointer click actuates once.
        for e in [
            GlassError::AxUnsupported,
            GlassError::AxActionUnavailable(3),
        ] {
            assert!(e.invoke_fallback_eligible(), "{e}");
        }
        // Everything else fails CLOSED — including the two that may mean "dispatched, outcome
        // unknown" (`AxActionFailed`, `AccessibilityUnavailable`, which carries the invoke
        // timeout), the drift/pre-check errors, and any variant not named at all (the wildcard).
        for e in [
            GlassError::AxActionFailed(3, "boom".into()),
            GlassError::AccessibilityUnavailable("invoke timed out".into()),
            GlassError::AxElementChanged(3),
            GlassError::AxElementGone(3),
            GlassError::NoAxSnapshot,
            GlassError::AxElementNotFound(3),
            GlassError::NoActiveSession,
            GlassError::Timeout(10),
            GlassError::Backend("bus died".into()),
            GlassError::Bounded {
                kind: BoundKind::TimedOut,
                whose: Whose::Callee,
                dispatch: BoundDispatch::MayHaveDispatched,
                message: "adb:shell: no answer within 10s".into(),
            },
            GlassError::ToolFailed {
                call: "adb shell input tap 1 2".into(),
                said: String::new(),
            },
        ] {
            assert!(!e.invoke_fallback_eligible(), "{e}");
        }
    }

    #[test]
    fn a_write_that_did_not_take_names_both_the_request_and_the_read_back() {
        // glass#363: an iOS field that autocapitalized the first letter took every keystroke. Each
        // value is asserted with the label that binds it — "contains both" also passes a message
        // that swapped them, which is the inverted diagnosis.
        let msg = GlassError::value_not_applied(13, "glasssmoke3", Some("Glasssmoke3")).to_string();
        assert!(msg.contains("element #13"), "{msg}");
        assert!(msg.contains("asked for \"glasssmoke3\""), "{msg}");
        assert!(msg.contains("holds \"Glasssmoke3\""), "{msg}");
    }

    #[test]
    fn a_backends_own_explanation_closes_the_message() {
        // The shared text is written for no backend in particular, so the mechanism and its remedy
        // come from the site that knows them (glass#405).
        let msg = GlassError::value_not_applied_because(
            13,
            "new",
            Some("old"),
            "ACTION_SET_TEXT cannot replace text already in a Compose field",
        )
        .to_string();
        assert!(msg.ends_with("ACTION_SET_TEXT cannot replace text already in a Compose field"));
        assert!(msg.contains("the write did not take effect —"), "{msg}");
    }

    #[test]
    fn a_verdict_with_no_backend_explanation_ends_at_the_shared_text() {
        // No trailing dash with nothing after it for a site that has nothing to add.
        let msg = GlassError::value_not_applied(13, "new", Some("old")).to_string();
        assert!(msg.ends_with("the write did not take effect"), "{msg}");
        // The three-outcome reading is the reason the variant carries both values; an edit that
        // trimmed the message back to its last clause would otherwise keep every test green.
        assert!(msg.contains("in another form"), "{msg}");
        assert!(msg.contains("holding part of it"), "{msg}");
    }

    #[test]
    fn an_empty_read_back_renders_as_an_empty_value() {
        // An empty read-back says the write arrived and left nothing — not the same answer as no
        // reading at all.
        let msg = GlassError::value_not_applied(13, "hello", Some("")).to_string();
        assert!(msg.contains("holds \"\""), "{msg}");
    }

    #[test]
    fn a_reading_nobody_took_does_not_render_as_a_value() {
        // `None` is a failed platform read or an element not found; rendering it as `""`, or as
        // "holds no value", states something about the element that nobody observed.
        let msg = GlassError::value_not_applied(13, "hello", None).to_string();
        assert!(msg.contains("could not be read back"), "{msg}");
        assert!(!msg.contains("holds"), "{msg}");
    }

    #[test]
    fn only_operation_specific_post_write_verdicts_invalidate_the_captured_value() {
        // These verdicts are reached only after the value mutation itself went out.
        assert!(
            GlassError::value_not_applied(3, "world", Some("hello"))
                .set_value_failed_after_writing()
        );
        // Everything else keeps the captured value: the pre-write rejections, generic transport
        // evidence that may describe a guard read, and any variant not named (the wildcard).
        for e in [
            GlassError::AxElementNotFound(3),
            GlassError::AxElementChanged(3),
            GlassError::AxElementGone(3),
            GlassError::AxElementNotEditable(3),
            GlassError::AxElementNotClickable(3),
            GlassError::AxUnsupported,
            GlassError::AccessibilityUnavailable("uiautomator dump not ready".into()),
            GlassError::Backend("adb died".into()),
            GlassError::Timeout(10),
            GlassError::Bounded {
                kind: BoundKind::TimedOut,
                whose: Whose::Callee,
                dispatch: BoundDispatch::NotDispatched,
                message: "the write was refused before dispatch".into(),
            },
            GlassError::Bounded {
                kind: BoundKind::TimedOut,
                whose: Whose::Caller,
                dispatch: BoundDispatch::MayHaveDispatched,
                message: "a pre-write snapshot may have gone out".into(),
            },
            GlassError::ToolFailed {
                call: "adb shell uiautomator dump /sdcard/x.xml".into(),
                said: String::new(),
            },
        ] {
            assert!(!e.set_value_failed_after_writing(), "{e}");
        }
    }

    #[test]
    fn an_unconfirmed_write_says_the_text_was_typed() {
        let e = GlassError::AxWriteUnconfirmed(
            7,
            "nothing in the tree carries its role and name".into(),
        );
        let msg = e.to_string();
        assert!(msg.contains("the write went out"), "{msg}");
        assert!(
            msg.contains("nothing in the tree carries its role and name"),
            "{msg}"
        );
        // The id is what a caller re-addresses; dropping `element #{0}` from the template left
        // every other assertion here green.
        assert!(msg.contains("element #7"), "names the element: {msg}");
        assert!(
            msg.contains("rather than"),
            "names what to do instead of retyping: {msg}"
        );
    }

    #[test]
    fn an_unconfirmed_write_counts_as_dispatched() {
        // The session must drop its cached value: the write went out, so the value it holds is stale.
        assert!(GlassError::AxWriteUnconfirmed(7, "x".into()).set_value_failed_after_writing());
    }

    #[test]
    fn a_source_less_unconfirmed_write_marks_possible_dispatch() {
        assert_eq!(
            GlassError::AxWriteUnconfirmed(7, "x".into()).bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn nested_prior_dispatch_cannot_be_downgraded_by_an_outer_preflight_annotation() {
        let error = GlassError::BeforeDispatch(Box::new(GlassError::AfterDispatch(Box::new(
            GlassError::Backend("later failure".into()),
        ))));

        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched),
            "an inner dispatch must dominate an outer not-dispatched annotation"
        );
    }

    #[test]
    fn a_pre_write_refusal_still_counts_as_not_dispatched() {
        // AxElementGone is raised on both sides of the write on Android, so it must stay out.
        assert!(!GlassError::AxElementGone(7).set_value_failed_after_writing());
        assert!(!GlassError::AxElementChanged(7).set_value_failed_after_writing());
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
    fn sandboxed_app_exited_message_hints_the_remedy() {
        let msg = GlassError::SandboxedAppExited(Some(2)).to_string();
        assert!(msg.contains("sandbox:\"off\""), "{msg}");
        assert!(msg.contains("$HOME"), "{msg}");
    }

    #[test]
    fn app_exited_during_discovery_picks_the_sandboxed_variant_when_sandboxed() {
        let err = GlassError::app_exited_during_discovery(Some(2), crate::SandboxLevel::Default);
        assert!(matches!(err, GlassError::SandboxedAppExited(Some(2))));
    }

    #[test]
    fn app_exited_during_discovery_picks_the_plain_variant_when_not_sandboxed() {
        let err = GlassError::app_exited_during_discovery(Some(2), crate::SandboxLevel::Off);
        assert!(matches!(err, GlassError::AppExited(Some(2))));
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
