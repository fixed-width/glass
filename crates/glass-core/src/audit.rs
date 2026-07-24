//! Actuation audit seam (platform-agnostic). `glass-core` *invokes* an injected
//! [`AuditSink`] after every actuation so the log is complete by construction; the
//! concrete JSONL writer + redaction policy live in `glass-mcp`. Data + trait only —
//! no serde/JSON/OS types, so the platform-agnostic invariant holds.

use std::time::Duration;

use crate::error::Result;
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
    /// Derive an outcome from a `glass-core` result (the error is stringified).
    pub fn from_result<T>(r: &Result<T>) -> Self {
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
        /// `ClickMethod::label()` of the path that actuated; `None` when the
        /// click errored before either path completed.
        method: Option<&'static str>,
    },
    SetValue {
        element: ElementRef,
        text: &'a str,
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
        let ok: Result<()> = Ok(());
        let o = AuditOutcome::from_result(&ok);
        assert!(o.ok && o.error.is_none());

        let err: Result<()> = Err(GlassError::NoActiveSession);
        let e = AuditOutcome::from_result(&err);
        assert!(!e.ok);
        assert!(e.error.unwrap().to_lowercase().contains("session"));
    }

    #[test]
    fn click_element_actuation_carries_the_actuating_method() {
        let element = ElementRef {
            id: 1,
            role: Some("Button".into()),
            name: Some("Save".into()),
        };
        let act = Actuation::ClickElement {
            element,
            method: Some("native-action"),
        };
        assert!(matches!(
            act,
            Actuation::ClickElement {
                method: Some("native-action"),
                ..
            }
        ));
    }
}
