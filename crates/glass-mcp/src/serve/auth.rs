//! Bearer-token authentication for the network transport.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;

/// Constant-time comparison of two byte slices. Returns false on length mismatch
/// (token length is not a meaningful secret here) without an early-out on content.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Does the `Authorization` header value carry the expected bearer token?
/// `header` is the raw header value (e.g. `Some("Bearer abc")`); `expected` is the
/// configured token. Matching is constant-time over the token bytes.
pub fn bearer_ok(header: Option<&str>, expected: &str) -> bool {
    let Some(h) = header else { return false };
    let Some(rest) = h.strip_prefix("Bearer ") else { return false };
    ct_eq(rest.trim().as_bytes(), expected.as_bytes())
}

/// axum middleware: require a valid bearer token when one is configured. With
/// `None` (loopback-open mode) every request passes. A failure is a bare `401`
/// with no body detail.
pub async fn require_bearer(
    State(expected): State<Arc<Option<String>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(token) = expected.as_ref() {
        let header = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if !bearer_ok(header, token) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exact() {
        assert!(bearer_ok(Some("Bearer s3cret"), "s3cret"));
    }
    #[test]
    fn rejects_wrong() {
        assert!(!bearer_ok(Some("Bearer nope"), "s3cret"));
    }
    #[test]
    fn rejects_missing_or_malformed() {
        assert!(!bearer_ok(None, "s3cret"));
        assert!(!bearer_ok(Some("s3cret"), "s3cret")); // no "Bearer " prefix
        assert!(!bearer_ok(Some("Basic s3cret"), "s3cret"));
    }
    #[test]
    fn rejects_length_mismatch() {
        assert!(!bearer_ok(Some("Bearer s3"), "s3cret"));
        assert!(!bearer_ok(Some("Bearer s3cretXX"), "s3cret"));
    }
}
