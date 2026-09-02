//! Sign-in codes and the remembered-device cookie.
//!
//! A 6-digit emailed code proves inbox control at sign-up and at log-in from a
//! browser the account has not used before. The code has a millionth of a
//! session token's entropy, so everything around it compensates: it is stored
//! peppered (a database leak alone cannot brute a million-value space against
//! HMAC with an absent key), bound to its challenge id (a code for one
//! challenge is noise for another), short-lived, and dead after a handful of
//! wrong guesses — the attempt cap lives in the same UPDATE that checks it.
//!
//! The device cookie is what keeps everyday logins one step. It is a signed
//! statement — "this browser completed a code for this account, until this
//! date" — not a credential: presenting it skips the code, never the password.
//! Stateless by design: there is nothing to store or revoke, and losing it
//! costs one extra email.
// ponytail: stateless device cookie, no revocation list; add a devices table
// if per-device revocation is ever asked for.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use cm_core::{AppError, Secret};
use std::time::Duration;
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// Six digits: what a person retypes from a phone screen without resenting it.
pub const CODE_LEN: usize = 6;
/// A code outlives the email round-trip and nothing else.
pub const CODE_TTL_SECS: i64 = 600;
/// Wrong guesses before the challenge dies.
pub const MAX_CODE_ATTEMPTS: i32 = 5;
/// How long a browser stays remembered.
pub const DEVICE_TTL: Duration = Duration::from_secs(90 * 86_400);

/// Same prefix rules as the session pair: host-bound, `Secure`, `Path=/`.
pub const DEVICE_COOKIE: &str = "__Host-cm_device";

/// A fresh 6-digit code, uniformly distributed.
pub fn generate_code() -> Result<String, AppError> {
    // Rejection sampling over u32, so no value is favoured by the modulo.
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes)
            .map_err(|error| AppError::internal(format!("the system RNG failed: {error}")))?;
        let value = u32::from_le_bytes(bytes);
        if value < 4_000_000_000 {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

/// The stored digest of a code, bound to its challenge.
pub fn code_hash(pepper: &Secret<String>, challenge_id: Uuid, code: &str) -> Vec<u8> {
    crate::hash::peppered(pepper, "login_code", &format!("{challenge_id}:{code}"))
}

/// The value of a remembered-device cookie: `user_id.expires_unix.signature`.
pub fn device_value(pepper: &Secret<String>, user_id: Uuid, expires_at: DateTime<Utc>) -> String {
    let expires = expires_at.timestamp();
    let signature = device_signature(pepper, user_id, expires);
    format!("{user_id}.{expires}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// The `Set-Cookie` for a browser that just completed a code.
pub fn device_cookie(value: &str) -> String {
    format!(
        "{DEVICE_COOKIE}={value}; Path=/; Secure; SameSite=Lax; HttpOnly; Max-Age={}",
        DEVICE_TTL.as_secs()
    )
}

/// Whether a presented device cookie remembers this account, now.
pub fn device_remembers(
    pepper: &Secret<String>,
    presented: &str,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> bool {
    let mut parts = presented.splitn(3, '.');
    let (Some(claimed_user), Some(claimed_expiry), Some(claimed_signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };

    let Ok(claimed_user) = claimed_user.parse::<Uuid>() else {
        return false;
    };
    let Ok(expires) = claimed_expiry.parse::<i64>() else {
        return false;
    };
    let Ok(claimed_signature) = URL_SAFE_NO_PAD.decode(claimed_signature) else {
        return false;
    };

    // Bound to the account logging in: another user's cookie on the same
    // browser says nothing about this one.
    if claimed_user != user_id || expires <= now.timestamp() {
        return false;
    }

    let expected = device_signature(pepper, user_id, expires);
    expected.as_slice().ct_eq(&claimed_signature).into()
}

fn device_signature(pepper: &Secret<String>, user_id: Uuid, expires_unix: i64) -> Vec<u8> {
    crate::hash::peppered(pepper, "device", &format!("{user_id}:{expires_unix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn pepper() -> Secret<String> {
        Secret::new("a-pepper-that-is-at-least-32-characters".to_owned())
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..100 {
            let code = generate_code().expect("generate");
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.chars().all(|c| c.is_ascii_digit()), "{code}");
        }
    }

    #[test]
    fn a_code_hash_is_bound_to_its_challenge() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert_ne!(
            code_hash(&pepper(), a, "123456"),
            code_hash(&pepper(), b, "123456"),
            "the same code under another challenge must not collide"
        );
    }

    #[test]
    fn a_device_cookie_round_trips_and_expires() {
        let user = Uuid::now_v7();
        let now = Utc::now();
        let value = device_value(&pepper(), user, now + ChronoDuration::days(90));

        assert!(device_remembers(&pepper(), &value, user, now));
        assert!(
            !device_remembers(&pepper(), &value, user, now + ChronoDuration::days(91)),
            "an expired statement is no statement"
        );
    }

    #[test]
    fn another_accounts_cookie_is_not_this_ones() {
        let user = Uuid::now_v7();
        let other = Uuid::now_v7();
        let now = Utc::now();
        let value = device_value(&pepper(), user, now + ChronoDuration::days(90));

        assert!(!device_remembers(&pepper(), &value, other, now));
    }

    /// The expiry rides in the clear, so moving it must break the signature —
    /// otherwise a stolen cookie could be made eternal with a text editor.
    #[test]
    fn a_tampered_expiry_is_refused() {
        let user = Uuid::now_v7();
        let now = Utc::now();
        let value = device_value(&pepper(), user, now + ChronoDuration::days(1));

        let mut parts: Vec<&str> = value.splitn(3, '.').collect();
        let far_future = (now + ChronoDuration::days(3650)).timestamp().to_string();
        parts[1] = &far_future;
        let forged = parts.join(".");

        assert!(!device_remembers(&pepper(), &forged, user, now));
    }

    #[test]
    fn garbage_is_refused_without_panicking() {
        for garbage in ["", "a.b.c", "not-a-cookie", "..", "x.y"] {
            assert!(!device_remembers(
                &pepper(),
                garbage,
                Uuid::now_v7(),
                Utc::now()
            ));
        }
    }
}
