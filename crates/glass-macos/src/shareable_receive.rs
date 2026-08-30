use std::sync::mpsc::RecvTimeoutError;

use glass_core::{GlassError, Result, Whose};

pub(crate) fn classify_receive<T>(received: std::result::Result<T, RecvTimeoutError>) -> Result<T> {
    match received {
        Ok(reply) => Ok(reply),
        Err(RecvTimeoutError::Timeout) => Err(GlassError::Backend(
            "SCShareableContent completion handler did not reply within the query timeout".into(),
        )),
        Err(RecvTimeoutError::Disconnected) => Err(GlassError::Backend(
            "SCShareableContent completion handler was dropped without replying".into(),
        )),
    }
}

pub(crate) fn classify_receive_by<T>(
    received: std::result::Result<T, RecvTimeoutError>,
    owner: Whose,
    operation: &str,
) -> Result<T> {
    match received {
        Err(RecvTimeoutError::Timeout) if owner == Whose::Caller => {
            Err(GlassError::caller_deadline_elapsed(operation))
        }
        received => classify_receive(received),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_is_returned_unchanged() {
        assert_eq!(classify_receive(Ok(42)).unwrap(), 42);
    }

    #[test]
    fn timeout_is_a_distinct_backend_failure() {
        let err = classify_receive::<()>(Err(RecvTimeoutError::Timeout)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "backend error: SCShareableContent completion handler did not reply within the query timeout"
        );
    }

    #[test]
    fn caller_owned_timeout_retains_deadline_and_dispatch_provenance() {
        let err = classify_receive_by::<()>(
            Err(RecvTimeoutError::Timeout),
            Whose::Caller,
            "macOS window list",
        )
        .unwrap_err();

        assert_eq!(err.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            err.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn disconnection_is_a_distinct_backend_failure() {
        let err = classify_receive::<()>(Err(RecvTimeoutError::Disconnected)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "backend error: SCShareableContent completion handler was dropped without replying"
        );
    }
}
