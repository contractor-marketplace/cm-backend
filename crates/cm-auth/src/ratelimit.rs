//! Rate-limit policies.
//!
//! Every limit is a fixed window counted in the database. Two properties matter
//! and are easy to get wrong:
//!
//! * The counter is committed independently of the work it guards. A failed
//!   login rolls back; its rate-limit increment must not, or the limit would
//!   only ever count successes.
//! * The bucket key is hashed with the pepper before it is stored, so the table
//!   never holds an IP address or a user id in the clear.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cm_core::{AppError, Secret};
use sqlx::PgPool;

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Prefixes the bucket key, so two policies over the same subject count
    /// separately.
    pub name: &'static str,
    pub limit: i32,
    pub window: ChronoDuration,
}

/// Registration is expensive (an Argon2 hash) and rarely repeated by a real
/// person.
pub fn register_per_ip() -> Policy {
    Policy {
        name: "register:ip",
        limit: 10,
        window: ChronoDuration::hours(1),
    }
}

/// Guards the whole login endpoint against an address working through a list of
/// accounts. Per-account lockout handles the other direction — one account,
/// many guesses — and the two are deliberately separate: an attacker with a
/// botnet defeats the first, and a shared office NAT would be punished by the
/// second if it were the only control.
pub fn login_per_ip() -> Policy {
    Policy {
        name: "login:ip",
        limit: 20,
        window: ChronoDuration::minutes(15),
    }
}

/// Federated sign-in costs a token verification and possibly a key fetch.
pub fn federated_sign_in_per_ip() -> Policy {
    Policy {
        name: "federated_sign_in:ip",
        limit: 30,
        window: ChronoDuration::minutes(15),
    }
}

/// Linking an identity is rare and irreversible-ish, so it is tightly bounded.
pub fn link_identity_per_user() -> Policy {
    Policy {
        name: "link_identity:user",
        limit: 5,
        window: ChronoDuration::hours(1),
    }
}

/// Password change verifies the current password, so it is an oracle if left
/// unbounded.
pub fn password_change_per_user() -> Policy {
    Policy {
        name: "password_change:user",
        limit: 5,
        window: ChronoDuration::hours(1),
    }
}

/// Count one request and return an error if the bucket is over its limit.
///
/// Runs on the pool rather than in a caller's transaction, so the count
/// survives the rollback of whatever it was guarding.
pub async fn enforce(
    pool: &PgPool,
    pepper: &Secret<String>,
    policy: Policy,
    subject: &str,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    let bucket = crate::hash::rate_limit_bucket(pepper, &format!("{}:{}", policy.name, subject));

    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    let decision =
        cm_db::repo::rate_limit::hit(&mut conn, &bucket, policy.limit, policy.window, now).await?;

    if decision.is_allowed() {
        return Ok(());
    }

    tracing::warn!(
        policy = policy.name,
        count = decision.count,
        limit = decision.limit,
        "rate limit exceeded"
    );

    Err(AppError::TooManyRequests {
        retry_after: decision.retry_after(now),
    })
}

/// How often the background sweeper runs, and how many rows it removes per
/// statement. Bounded so the delete never becomes a long lock-holding
/// transaction on a box that is also serving requests.
pub const SWEEP_INTERVAL_SECS: u64 = 300;
pub const SWEEP_BATCH: i64 = 5_000;
/// Stops a single sweep from running unboundedly if the table is far behind.
pub const SWEEP_MAX_BATCHES: usize = 20;

/// Delete elapsed windows in bounded batches. Returns how many rows went.
pub async fn sweep(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    let mut removed = 0;

    for _ in 0..SWEEP_MAX_BATCHES {
        let batch = cm_db::repo::rate_limit::sweep_expired(&mut conn, now, SWEEP_BATCH).await?;
        removed += batch;
        if batch < SWEEP_BATCH as u64 {
            break;
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_are_distinct_and_bounded() {
        let policies = [
            register_per_ip(),
            login_per_ip(),
            federated_sign_in_per_ip(),
            link_identity_per_user(),
            password_change_per_user(),
        ];

        for policy in policies {
            assert!(policy.limit > 0, "{} has no limit", policy.name);
            assert!(
                policy.window > ChronoDuration::zero(),
                "{} has no window",
                policy.name
            );
        }

        let names: std::collections::HashSet<&str> =
            policies.iter().map(|policy| policy.name).collect();
        assert_eq!(names.len(), policies.len(), "policy names must be unique");
    }
}
