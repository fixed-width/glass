use std::sync::mpsc::RecvTimeoutError;

use glass_core::{GlassError, Result};

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
    fn disconnection_is_a_distinct_backend_failure() {
        let err = classify_receive::<()>(Err(RecvTimeoutError::Disconnected)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "backend error: SCShareableContent completion handler was dropped without replying"
        );
    }
}
