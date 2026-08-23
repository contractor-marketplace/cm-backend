//! Keyed and unkeyed digests.
//!
//! Two different jobs, deliberately kept apart:
//!
//! * Session tokens are hashed with plain SHA-256. The token already carries
//!   256 bits of entropy, so there is nothing to brute-force and a pepper would
//!   add only a key to lose.
//! * Values that are *guessable* — an IP address, a user id used as a
//!   rate-limit key — are peppered. Their input space is small enough to
//!   enumerate, so an unkeyed digest of one is barely better than storing it.

use cm_core::Secret;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// SHA-256 of a high-entropy secret, for storage and lookup.
pub fn digest_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// A keyed digest, domain-separated so the same pepper can serve several uses
/// without one being usable to forge another.
pub fn peppered(pepper: &Secret<String>, domain: &str, value: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(pepper.expose().as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(domain.as_bytes());
    mac.update(b"\x00");
    mac.update(value.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Digest of a client address, for `sessions.ip_hash` and `audit_log.ip_hash`.
pub fn ip(pepper: &Secret<String>, address: &str) -> Vec<u8> {
    peppered(pepper, "ip", address)
}

/// Digest of a rate-limit bucket key.
pub fn rate_limit_bucket(pepper: &Secret<String>, bucket: &str) -> Vec<u8> {
    peppered(pepper, "ratelimit", bucket)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pepper(value: &str) -> Secret<String> {
        Secret::new(value.to_owned())
    }

    #[test]
    fn token_digests_are_sha256_and_stable() {
        let digest = digest_token("hello");
        assert_eq!(digest.len(), 32);
        assert_eq!(digest, digest_token("hello"));
        assert_ne!(digest, digest_token("hellp"));
    }

    #[test]
    fn peppered_digests_depend_on_the_pepper() {
        let a = ip(&pepper("pepper-one-that-is-long-enough!!"), "203.0.113.7");
        let b = ip(&pepper("pepper-two-that-is-long-enough!!"), "203.0.113.7");
        assert_ne!(a, b, "a different pepper must give a different digest");
    }

    /// Without domain separation, an IP digest and a rate-limit digest of the
    /// same string would collide, and a bucket could be predicted from an
    /// address digest that happened to leak.
    #[test]
    fn domains_are_separated() {
        let secret = pepper("a-pepper-that-is-at-least-32-chars!!");
        assert_ne!(
            ip(&secret, "203.0.113.7"),
            rate_limit_bucket(&secret, "203.0.113.7")
        );
    }

    /// The digest must not be a prefix-concatenation, or `("ip", "ab")` and
    /// `("ipa", "b")` would be the same value.
    #[test]
    fn the_domain_boundary_is_unambiguous() {
        let secret = pepper("a-pepper-that-is-at-least-32-chars!!");
        assert_ne!(peppered(&secret, "ip", "ab"), peppered(&secret, "ipa", "b"));
    }
}
