//! Argon2id hashing and password policy.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use cm_core::AppError;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// OWASP's Argon2id baseline: 19 MiB, two passes, one lane, 32-byte output.
/// Stated explicitly rather than taken from the crate default so a dependency
/// bump cannot quietly change the cost of every stored hash.
const M_COST_KIB: u32 = 19 * 1024;
const T_COST: u32 = 2;
const P_COST: u32 = 1;
const OUTPUT_LEN: usize = 32;

/// Minimum password length. Length is the only policy that reliably buys
/// entropy; composition rules mostly buy predictable substitutions.
pub const MIN_PASSWORD_LEN: usize = 12;
/// Upper bound. Argon2's cost does not depend on input length, so this exists
/// only to stop a caller streaming megabytes into a hash function.
pub const MAX_PASSWORD_LEN: usize = 1024;

/// Long passwords that are nonetheless among the first anyone tries. The length
/// rule already removes most of the classic list; these are the ones that
/// survive it.
const DENYLIST: &[&str] = &[
    "123456789012",
    "1234567890123",
    "12345678901234",
    "123456789012345",
    "1234567890123456",
    "passwordpassword",
    "password123456",
    "password1234567",
    "qwertyuiopasdfgh",
    "qwertyuiop123456",
    "adminadminadmin",
    "letmeinletmein",
    "iloveyouiloveyou",
    "welcome123456",
    "abcdefghijklmnop",
    "aaaaaaaaaaaa",
    "111111111111",
    "000000000000",
    "trustno1trustno1",
    "correcthorsebatterystaple",
];

/// Hashes and verifies passwords, with a bound on how many run at once.
///
/// Each hash holds 19 MiB for its duration, so an unbounded burst of logins is
/// a memory-exhaustion vector on a small box, not merely a slow one. The
/// semaphore turns that into a queue.
#[derive(Clone)]
pub struct PasswordHasherService {
    permits: Arc<Semaphore>,
    /// Verified against when the account does not exist, so that an unknown
    /// address costs the same as a wrong password.
    decoy_hash: Arc<String>,
}

impl std::fmt::Debug for PasswordHasherService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordHasherService")
            .field("available_permits", &self.permits.available_permits())
            .finish_non_exhaustive()
    }
}

fn argon2() -> Result<Argon2<'static>, AppError> {
    let params = Params::new(M_COST_KIB, T_COST, P_COST, Some(OUTPUT_LEN))
        .map_err(|error| AppError::internal(format!("invalid Argon2 parameters: {error}")))?;

    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

fn hash_blocking(password: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    argon2()?
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| AppError::internal(format!("hashing failed: {error}")))
}

fn verify_blocking(password: &str, phc: &str) -> Result<bool, AppError> {
    let parsed = PasswordHash::new(phc)
        .map_err(|error| AppError::internal(format!("stored hash is unreadable: {error}")))?;

    match argon2()?.verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(error) => Err(AppError::internal(format!("verification failed: {error}"))),
    }
}

impl PasswordHasherService {
    /// Build the service, generating the decoy hash once.
    ///
    /// The decoy is a hash of a random value nobody knows, so verifying against
    /// it always fails while costing exactly what a real verification costs.
    pub fn new(max_concurrency: usize) -> Result<Self, AppError> {
        let filler = crate::token::generate()?;
        let decoy_hash = hash_blocking(&filler)?;

        Ok(Self {
            permits: Arc::new(Semaphore::new(max_concurrency.max(1))),
            decoy_hash: Arc::new(decoy_hash),
        })
    }

    /// Reject a password before it reaches the hasher.
    pub fn check_policy(password: &str, email: &str) -> Result<(), AppError> {
        if password.chars().count() < MIN_PASSWORD_LEN {
            return Err(AppError::invalid(format!(
                "Password must be at least {MIN_PASSWORD_LEN} characters."
            )));
        }
        if password.len() > MAX_PASSWORD_LEN {
            return Err(AppError::invalid(format!(
                "Password must be at most {MAX_PASSWORD_LEN} bytes."
            )));
        }

        let lowered = password.to_lowercase();
        if DENYLIST.contains(&lowered.as_str()) {
            return Err(AppError::invalid(
                "That password is too common. Choose something less guessable.",
            ));
        }

        // The address is public, so a password built from it is public too.
        if let Some(local_part) = email.split('@').next() {
            let local_part = local_part.trim().to_lowercase();
            if local_part.chars().count() >= 4 && lowered.contains(&local_part) {
                return Err(AppError::invalid(
                    "Password must not contain your email address.",
                ));
            }
        }

        Ok(())
    }

    async fn run_bounded<T, F>(&self, work: F) -> Result<T, AppError>
    where
        F: FnOnce() -> Result<T, AppError> + Send + 'static,
        T: Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(AppError::internal)?;

        // Off the async runtime: Argon2 blocks a worker thread for tens of
        // milliseconds, which is long enough to stall unrelated requests.
        let result = tokio::task::spawn_blocking(move || {
            let outcome = work();
            drop(permit);
            outcome
        })
        .await
        .map_err(AppError::internal)?;

        result
    }

    pub async fn hash(&self, password: &str) -> Result<String, AppError> {
        let password = password.to_owned();
        self.run_bounded(move || hash_blocking(&password)).await
    }

    pub async fn verify(&self, password: &str, phc: &str) -> Result<bool, AppError> {
        let password = password.to_owned();
        let phc = phc.to_owned();
        self.run_bounded(move || verify_blocking(&password, &phc))
            .await
    }

    /// Burn the same work a real verification would, and always fail.
    ///
    /// Called when no account matches, so that "unknown address" and "wrong
    /// password" cannot be told apart by how long the answer took.
    pub async fn verify_decoy(&self, password: &str) -> Result<(), AppError> {
        let decoy = self.decoy_hash.clone();
        let _ = self.verify(password, &decoy).await?;
        Ok(())
    }

    /// Whether a stored hash was produced with weaker parameters than the
    /// current ones, and should be replaced on the next successful login.
    pub fn needs_rehash(phc: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(phc) else {
            return true;
        };
        let Ok(params) = Params::try_from(&parsed) else {
            return true;
        };

        params.m_cost() < M_COST_KIB || params.t_cost() < T_COST || params.p_cost() != P_COST
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> PasswordHasherService {
        PasswordHasherService::new(2).expect("build hasher")
    }

    #[tokio::test]
    async fn a_password_round_trips() {
        let service = service();
        let phc = service
            .hash("correct battery staple 9")
            .await
            .expect("hash");

        assert!(phc.starts_with("$argon2id$"), "{phc}");
        assert!(service
            .verify("correct battery staple 9", &phc)
            .await
            .expect("verify"));
        assert!(!service
            .verify("correct battery staple 8", &phc)
            .await
            .expect("verify"));
    }

    #[tokio::test]
    async fn the_stored_hash_uses_the_documented_parameters() {
        let phc = service()
            .hash("a sufficiently long password")
            .await
            .expect("hash");

        assert!(phc.contains("m=19456"), "{phc}");
        assert!(phc.contains("t=2"), "{phc}");
        assert!(phc.contains("p=1"), "{phc}");
        assert!(!PasswordHasherService::needs_rehash(&phc));
    }

    #[tokio::test]
    async fn the_same_password_hashes_differently_every_time() {
        let service = service();
        let first = service
            .hash("a sufficiently long password")
            .await
            .expect("hash");
        let second = service
            .hash("a sufficiently long password")
            .await
            .expect("hash");

        assert_ne!(first, second, "each hash must carry its own salt");
    }

    #[tokio::test]
    async fn a_weaker_stored_hash_is_flagged_for_rehashing() {
        let weak = Params::new(8 * 1024, 1, 1, Some(32)).expect("params");
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        let phc = Argon2::new(Algorithm::Argon2id, Version::V0x13, weak)
            .hash_password(b"a sufficiently long password", &salt)
            .expect("hash")
            .to_string();

        assert!(PasswordHasherService::needs_rehash(&phc));
        assert!(PasswordHasherService::needs_rehash("not a phc string"));
    }

    #[tokio::test]
    async fn the_decoy_always_fails_but_still_does_the_work() {
        service()
            .verify_decoy("whatever was typed")
            .await
            .expect("must not error");
    }

    #[test]
    fn policy_rejects_short_common_and_self_referential_passwords() {
        let check =
            |password: &str| PasswordHasherService::check_policy(password, "marisol@example.com");

        assert!(check("short").is_err(), "under the length floor");
        assert!(check("passwordpassword").is_err(), "on the denylist");
        assert!(
            check("PasswordPassword").is_err(),
            "denylist is case-insensitive"
        );
        assert!(
            check("marisol-is-my-password").is_err(),
            "contains the local part"
        );
        assert!(check("a sufficiently long password").is_ok());
        assert!(
            check(&"x".repeat(MAX_PASSWORD_LEN + 1)).is_err(),
            "over the ceiling"
        );
    }

    /// A three-character local part like `ab@x.com` would otherwise ban every
    /// password containing those letters in sequence.
    #[test]
    fn a_very_short_local_part_does_not_ban_common_letter_runs() {
        assert!(
            PasswordHasherService::check_policy("abc is a fine start", "abc@example.com").is_ok()
        );
    }

    #[test]
    fn the_length_floor_counts_characters_not_bytes() {
        // Eleven characters, but well over twelve bytes.
        assert!(PasswordHasherService::check_policy("ααααααααααα", "x@example.com").is_err());
        assert!(PasswordHasherService::check_policy("αααααααααααα", "x@example.com").is_ok());
    }
}
