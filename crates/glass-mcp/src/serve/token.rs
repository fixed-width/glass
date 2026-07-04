//! Token generation for `glass-mcp gen-token`.

use base64::Engine;
use rand::RngCore;

/// Generate a fresh ~256-bit token, base64url (no padding), single line.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_urlsafe_and_long() {
        let t = generate_token();
        // 32 bytes → 43 base64url chars (no padding).
        assert_eq!(t.len(), 43);
        assert!(t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(!t.contains('\n') && !t.contains('='));
    }

    #[test]
    fn tokens_differ() {
        assert_ne!(generate_token(), generate_token());
    }
}
