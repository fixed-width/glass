//! Actuation audit seam (platform-agnostic). `glass-core` *invokes* an injected
//! [`AuditSink`] after every actuation so the log is complete by construction; the
//! concrete JSONL writer + redaction policy live in `glass-mcp`. Data + trait only —
//! no serde/JSON/OS types, so the platform-agnostic invariant holds.

use std::time::Duration;

use crate::platform::{AppSpec, KeyEvent, PointerEvent, WindowOp};

/// The active window an actuation was directed at (best-effort).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRef {
    pub id: u64,
    pub title: Option<String>,
}

/// An accessibility element an actuation targeted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementRef {
    pub id: u32,
    pub role: Option<String>,
    pub name: Option<String>,
}

/// Ambient context for an actuation.
#[derive(Clone, Debug, Default)]
pub struct ActuationContext {
    pub window: Option<WindowRef>,
}

/// Whether an actuation succeeded, and the error message if not.
#[derive(Clone, Debug)]
pub struct AuditOutcome {
    pub ok: bool,
    pub error: Option<String>,
}

impl AuditOutcome {
    /// Derive an outcome from a result whose error can be safely stringified.
    pub fn from_result<T, E: std::fmt::Display>(r: &std::result::Result<T, E>) -> Self {
        match r {
            Ok(_) => AuditOutcome {
                ok: true,
                error: None,
            },
            Err(e) => AuditOutcome {
                ok: false,
                error: Some(e.to_string()),
            },
        }
    }
}

/// One actuation as seen at the core choke-point. Borrows the originating typed
/// event so the sink can format without `glass-core` depending on serde/JSON.
#[derive(Debug)]
pub enum Actuation<'a> {
    Launch {
        spec: &'a AppSpec,
        backend: &'a str,
    },
    Stop,
    Pointer {
        event: &'a PointerEvent,
    },
    Key {
        event: &'a KeyEvent,
    },
    ClipboardSet {
        text: &'a str,
    },
    Window {
        op: &'a WindowOp,
    },
    ClickElement {
        element: ElementRef,
        mode: &'a str,
        method: Option<&'a str>,
        native_fallback: Option<&'a str>,
        actuated_id: Option<u32>,
        dispatch: &'a str,
        confirmation: &'a str,
    },
    SetValue {
        element: ElementRef,
        text: &'a str,
        dispatch: &'a str,
        confirmation: &'a str,
    },
    TypeTarget {
        element: ElementRef,
        text: &'a str,
        focus_mode: &'a str,
        focus_method: Option<&'a str>,
        focus_dispatch: &'a str,
        focus_confirmation: &'a str,
        type_dispatch: &'a str,
    },
}

/// Receives every actuation. Implemented in `glass-mcp` (`JsonlSink`). `Send` so it
/// can live on `Glass`, which moves across the runtime's worker thread.
pub trait AuditSink: Send {
    /// Record one actuation. Implementations **must not panic** and must not
    /// propagate errors: a sink-internal failure (e.g. I/O) is handled internally
    /// (logged/counted/fail-closed), never surfaced into the actuation path.
    fn record(
        &self,
        act: &Actuation,
        ctx: &ActuationContext,
        outcome: &AuditOutcome,
        dur: Duration,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::GlassError;

    #[test]
    fn outcome_from_result_captures_ok_and_error() {
        let ok: std::result::Result<(), &str> = Ok(());
        let o = AuditOutcome::from_result(&ok);
        assert!(o.ok && o.error.is_none());

        let err: std::result::Result<(), GlassError> = Err(GlassError::NoActiveSession);
        let e = AuditOutcome::from_result(&err);
        assert!(!e.ok);
        assert!(e.error.unwrap().to_lowercase().contains("session"));
    }

    #[test]
    fn click_element_actuation_carries_safe_semantic_dispatch_metadata() {
        let element = ElementRef {
            id: 1,
            role: Some("Button".into()),
            name: Some("Save".into()),
        };
        let act = Actuation::ClickElement {
            element: element.clone(),
            mode: "auto",
            method: Some("pointer"),
            native_fallback: Some("target exposes no native accessibility action"),
            actuated_id: None,
            dispatch: "dispatched",
            confirmation: "dispatch_confirmed",
        };
        let Actuation::ClickElement {
            element: got_element,
            mode,
            method,
            native_fallback,
            actuated_id,
            dispatch,
            confirmation,
        } = act
        else {
            panic!("wrong variant");
        };
        assert_eq!(got_element, element);
        assert_eq!(mode, "auto");
        assert_eq!(method, Some("pointer"));
        assert_eq!(
            native_fallback,
            Some("target exposes no native accessibility action")
        );
        assert_eq!(actuated_id, None);
        assert_eq!(dispatch, "dispatched");
        assert_eq!(confirmation, "dispatch_confirmed");
    }

    #[test]
    fn type_target_actuation_carries_focus_and_type_dispatch_metadata() {
        let element = ElementRef {
            id: 7,
            role: Some("TextField".into()),
            name: Some("Account name".into()),
        };
        let act = Actuation::TypeTarget {
            element: element.clone(),
            text: "submitted text",
            focus_mode: "auto",
            focus_method: Some("native_action"),
            focus_dispatch: "dispatched",
            focus_confirmation: "focus_confirmed",
            type_dispatch: "dispatched",
        };
        let Actuation::TypeTarget {
            element: got_element,
            text,
            focus_mode,
            focus_method,
            focus_dispatch,
            focus_confirmation,
            type_dispatch,
        } = act
        else {
            panic!("wrong variant");
        };
        assert_eq!(got_element, element);
        assert_eq!(text, "submitted text");
        assert_eq!(focus_mode, "auto");
        assert_eq!(focus_method, Some("native_action"));
        assert_eq!(focus_dispatch, "dispatched");
        assert_eq!(focus_confirmation, "focus_confirmed");
        assert_eq!(type_dispatch, "dispatched");
    }
}
