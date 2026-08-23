//! Opaque token generation.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use cm_core::AppError;

/// 256 bits. Enough that guessing is not a threat model, and the stored SHA-256
/// digest is not reversible by enumeration.
pub const TOKEN_BYTES: usize = 32;

/// A fresh session token, URL-safe and cookie-safe.
///
/// The raw value is returned exactly once, to be put in a `Set-Cookie`. Only its
/// digest is ever stored.
pub fn generate() -> Result<String, AppError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| AppError::internal(format!("the system RNG failed: {error}")))?;

    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn tokens_are_url_safe_and_long_enough() {
        let token = generate().expect("generate");

        // 32 bytes base64url without padding.
        assert_eq!(token.len(), 43);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "token must be safe in a cookie without quoting: {token}"
        );
    }

    #[test]
    fn tokens_do_not_repeat() {
        let tokens: HashSet<String> = (0..2_000).map(|_| generate().expect("generate")).collect();
        assert_eq!(tokens.len(), 2_000);
    }
}
