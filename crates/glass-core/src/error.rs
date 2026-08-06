use thiserror::Error;

/// Which bound ended a call that produced no answer — the distinction
/// [`crate::run_bounded_until`] makes and [`GlassError::Bounded`] carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundKind {
    /// The call ran and was killed when its effective bound elapsed — its own budget or the
    /// deadline it shares, whichever was nearer.
    ///
    /// It does not say which of the two governed, so a caller that passed a deadline must still
    /// compare them: its own budget firing is evidence the tool is wedged, and its caller's is
    /// evidence of nothing at all (glass#341, glass#347).
    TimedOut,
    /// The call never ran: the deadline it shares with the rest of a sequence was already spent.
    /// Nothing was asked, so nothing about the tool is known.
    NotStarted,
}

/// All fallible glass-core operations return this error.
///
/// Variants map to the actionable error kinds the MCP layer surfaces to the
/// agent. Backend crates fold their OS-specific failures into `Backend`.
#[derive(Debug, Error)]
pub enum GlassError {
    #[error("no active session — call glass_start to launch an app first")]
    NoActiveSession,

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

    #[error(
        "set_value on element #{0} did not take — the element does not hold the requested value. On \
         a desktop backend this usually means a read-only accessibility projection, so try \
         keystrokes; on Android or the iOS Simulator the write already IS keystrokes, so the tap may \
         have missed or the field rejected the input — re-snapshot to see what it holds"
    )]
    AxValueNotApplied(u32),

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

    /// A backend failure the backend itself reported. Two neighbours display identically and this
    /// variant is false for both — [`Self::Bounded`] for a call that ended at one of glass's own
    /// bounds, [`Self::ToolFailed`] for a tool that ran and exited non-zero. Classify with
    /// [`Self::bound`] and [`Self::tool_said`] rather than by matching this variant.
    #[error("backend error: {0}")]
    Backend(String),

    /// A tool glass drove ran and exited non-zero, carrying what it wrote to stderr.
    ///
    /// `said` is that stderr, trimmed — empty when the tool failed without a word, which for
    /// `uiautomator` is a crash whose trace went to the platform log instead (glass#341), and
    /// waiting resolves it where an explained failure will not. Read it through [`Self::tool_said`]
    /// rather than out of the rendered message, which the tool's own output is free to imitate
    /// (glass#348). A tool that explains itself on *stdout* reads as silent here.
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
    Bounded { kind: BoundKind, message: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl GlassError {
    /// Whether a failed native-invoke attempt may safely fall back to the synthetic
    /// pointer path. True only for outcomes where **no native action was dispatched**:
    /// the backend has no invoke at all ([`GlassError::AxUnsupported`]), or the element
    /// exposes no activation action ([`GlassError::AxActionUnavailable`]).
    ///
    /// Everything else fails CLOSED. A failure *after* dispatch — or an ambiguous
    /// transport/timeout error, which cannot be distinguished from one — must propagate,
    /// because a pointer click on top of a native action that may still land actuates the
    /// control twice (submitting a form twice, sending a message twice). The wildcard arm
    /// is deliberate: a new error variant is treated as "may have dispatched" until it is
    /// proven otherwise.
    pub fn invoke_fallback_eligible(&self) -> bool {
        matches!(
            self,
            GlassError::AxUnsupported | GlassError::AxActionUnavailable(_)
        )
    }

    /// Whether a failed value-write is **proven** to have gone out before failing — the only case
    /// where the session's captured value is stale and must be dropped. True for the read-back
    /// verdict [`GlassError::AxValueNotApplied`], raised only after a write was dispatched; false
    /// for everything else, including variants added later.
    ///
    /// Same "did anything dispatch?" question as [`Self::invoke_fallback_eligible`], but decided by
    /// an allowlist, because the two mistakes are not symmetric. Keeping the value can only make
    /// the guard reject more — an `AxElementChanged` whose own message tells the caller to
    /// re-snapshot, recoverable. Dropping it can only make the guard accept more, and what it then
    /// accepts is a write onto the wrong element, reported as `Ok`.
    ///
    /// Some verdicts cannot be classified at all: Android raises `ToolFailed`, `Bounded` and
    /// `AccessibilityUnavailable` on *both* sides of the dispatch — its pre-write re-snapshot and
    /// adb handshake fail the same way its post-write read-back does — so no variant-level split
    /// separates them, and the recoverable answer has to win.
    pub fn set_value_failed_after_writing(&self) -> bool {
        matches!(self, GlassError::AxValueNotApplied(_))
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
    /// that wrote a bare newline into a refusal glass stops retrying.
    pub fn tool_said(&self) -> Option<&str> {
        match self {
            GlassError::ToolFailed { said, .. } => Some(said.trim()),
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
        // A crash that manages a bare newline is still a crash. Trimmed on the way out as well as
        // on the way in, so a producer that forgets cannot turn one into a refusal glass stops
        // retrying.
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
    fn only_a_proven_post_dispatch_verdict_invalidates_the_captured_value() {
        // The read-back verdict is reached only after the write went out.
        assert!(GlassError::AxValueNotApplied(3).set_value_failed_after_writing());
        // Everything else keeps the captured value: the pre-dispatch rejections, the transport
        // errors raised on either side of the dispatch, and any variant not named (the wildcard).
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
                message: "adb:uiautomator dump: no answer within 20s".into(),
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
