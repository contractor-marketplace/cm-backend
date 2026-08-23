//! CSRF protection for state-changing authenticated requests.
//!
//! Not a plain double-submit. The expected token is *derived* from the session
//! id with a keyed hash, so the server never trusts the cookie it was given: it
//! recomputes what the token should be and compares. An attacker who can write
//! a cookie on the site's host — the usual way double-submit falls over — still
//! cannot produce a value that matches, because they do not have the pepper.
//!
//! Two independent checks run on every protected request:
//!
//! 1. The `X-CM-CSRF` header must equal the token derived from this session.
//! 2. `Origin`, when present, must equal the configured site origin.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use cm_core::{AppError, Origin, Secret};
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// The header a client echoes the token back in.
pub const CSRF_HEADER: &str = "x-cm-csrf";

/// The token a given session must present.
pub fn token_for_session(pepper: &Secret<String>, session_id: Uuid) -> String {
    let digest = crate::hash::peppered(pepper, "csrf", &session_id.to_string());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Whether a method changes state and therefore needs the check.
///
/// The safe methods are the ones a browser will issue cross-site as an ordinary
/// navigation; requiring a header on those would break links without adding
/// protection, since they must not change state anyway.
pub fn method_requires_check(method: &http::Method) -> bool {
    !matches!(
        *method,
        http::Method::GET | http::Method::HEAD | http::Method::OPTIONS | http::Method::TRACE
    )
}

/// Verify a request's CSRF token and origin.
pub fn verify(
    pepper: &Secret<String>,
    site_origin: &Origin,
    session_id: Uuid,
    presented: Option<&str>,
    origin_header: Option<&str>,
) -> Result<(), AppError> {
    // An Origin that is present and wrong is decisive: no legitimate same-site
    // request carries someone else's origin. An absent Origin is not treated as
    // failure — some same-origin requests omit it — which is exactly why the
    // token check below is the primary defence rather than a supplement.
    if let Some(origin) = origin_header {
        let matches = Origin::parse(origin)
            .map(|parsed| parsed.as_str() == site_origin.as_str())
            .unwrap_or(false);

        if !matches {
            tracing::warn!(
                presented_origin = %origin,
                "rejected a cross-origin state-changing request"
            );
            return Err(AppError::Forbidden);
        }
    }

    let Some(presented) = presented else {
        return Err(AppError::Forbidden);
    };

    let expected = token_for_session(pepper, session_id);
    if expected.as_bytes().ct_eq(presented.as_bytes()).into() {
        Ok(())
    } else {
        tracing::warn!("rejected a state-changing request with a bad CSRF token");
        Err(AppError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pepper() -> Secret<String> {
        Secret::new("a-pepper-that-is-at-least-32-characters".to_owned())
    }

    fn origin() -> Origin {
        Origin::parse("https://app.example.com").expect("origin")
    }

    #[test]
    fn tokens_are_bound_to_the_session_and_the_pepper() {
        let one = Uuid::now_v7();
        let two = Uuid::now_v7();

        assert_eq!(
            token_for_session(&pepper(), one),
            token_for_session(&pepper(), one)
        );
        assert_ne!(
            token_for_session(&pepper(), one),
            token_for_session(&pepper(), two)
        );

        let other = Secret::new("a-different-pepper-at-least-32-chars".to_owned());
        assert_ne!(
            token_for_session(&pepper(), one),
            token_for_session(&other, one)
        );
    }

    #[test]
    fn only_state_changing_methods_are_checked() {
        for method in [http::Method::GET, http::Method::HEAD, http::Method::OPTIONS] {
            assert!(!method_requires_check(&method), "{method}");
        }
        for method in [
            http::Method::POST,
            http::Method::PUT,
            http::Method::PATCH,
            http::Method::DELETE,
        ] {
            assert!(method_requires_check(&method), "{method}");
        }
    }

    #[test]
    fn a_correct_token_passes() {
        let session = Uuid::now_v7();
        let token = token_for_session(&pepper(), session);

        verify(&pepper(), &origin(), session, Some(&token), None).expect("should pass");
        verify(
            &pepper(),
            &origin(),
            session,
            Some(&token),
            Some("https://app.example.com"),
        )
        .expect("should pass with a matching origin");
    }

    #[test]
    fn a_missing_or_wrong_token_is_rejected() {
        let session = Uuid::now_v7();

        assert!(verify(&pepper(), &origin(), session, None, None).is_err());
        assert!(verify(&pepper(), &origin(), session, Some("nonsense"), None).is_err());
        assert!(verify(&pepper(), &origin(), session, Some(""), None).is_err());
    }

    /// The token of a *different* session must not work: this is what stops one
    /// authenticated user from driving another's browser.
    #[test]
    fn another_sessions_token_is_rejected() {
        let mine = Uuid::now_v7();
        let theirs = Uuid::now_v7();
        let their_token = token_for_session(&pepper(), theirs);

        assert!(verify(&pepper(), &origin(), mine, Some(&their_token), None).is_err());
    }

    #[test]
    fn a_foreign_or_unparseable_origin_is_rejected_even_with_a_good_token() {
        let session = Uuid::now_v7();
        let token = token_for_session(&pepper(), session);

        for bad in [
            "https://evil.example.com",
            "http://app.example.com",
            "null",
            "not-an-origin",
        ] {
            assert!(
                verify(&pepper(), &origin(), session, Some(&token), Some(bad)).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    /// Browsers normalise away the default port; the check must too, or an
    /// explicit `:443` would look like a different site.
    #[test]
    fn an_origin_differing_only_by_default_port_still_matches() {
        let session = Uuid::now_v7();
        let token = token_for_session(&pepper(), session);

        verify(
            &pepper(),
            &origin(),
            session,
            Some(&token),
            Some("https://app.example.com:443"),
        )
        .expect("the default port is not a different origin");
    }
}
