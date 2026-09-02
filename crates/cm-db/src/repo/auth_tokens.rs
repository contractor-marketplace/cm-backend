//! Email-borne credentials: sign-in codes and password-reset tokens.
//!
//! The table has existed since 0005; this module is its first writer. Two
//! shapes share it: a 6-digit code addressed by challenge id (low entropy, so
//! it carries an attempt counter and dies fast), and a reset link addressed by
//! its digest (256 bits, so the digest alone is the lookup). Both are
//! single-use, enforced by the same atomic consume-on-read UPDATE.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    EmailVerify,
    LoginCode,
    PasswordReset,
}

impl Purpose {
    pub const ALL: [Self; 3] = [Self::EmailVerify, Self::LoginCode, Self::PasswordReset];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmailVerify => "email_verify",
            Self::LoginCode => "login_code",
            Self::PasswordReset => "password_reset",
        }
    }
}

/// Issue a token, consuming any open ones the account holds for the same
/// purpose first — a fresh code or link always invalidates its predecessor,
/// so at most one is ever live per (user, purpose).
///
/// The caller supplies the id because a code's digest is bound to it: the id
/// has to exist before the hash can be computed.
pub async fn issue(
    conn: &mut PgConnection,
    id: Uuid,
    user_id: Uuid,
    purpose: Purpose,
    token_hash: &[u8],
    ttl_secs: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE auth_tokens SET consumed_at = now(), updated_at = now() \
          WHERE user_id = $1 AND purpose = $2 AND consumed_at IS NULL",
    )
    .bind(user_id)
    .bind(purpose.as_str())
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    sqlx::query(
        "INSERT INTO auth_tokens (id, user_id, purpose, token_hash, expires_at) \
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))",
    )
    .bind(id)
    .bind(user_id)
    .bind(purpose.as_str())
    .bind(token_hash)
    .bind(ttl_secs as f64)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// What one guess at a code produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeOutcome {
    /// The code matched; the challenge is consumed.
    Matched { user_id: Uuid },
    /// Wrong code; the challenge survives unless this was the last attempt.
    Wrong,
    /// No such challenge, or it is expired, consumed, or out of attempts.
    Gone,
}

/// Spend one attempt on a challenge, atomically.
///
/// One UPDATE decides everything: the attempt is counted, a match consumes the
/// token, and the final failed attempt consumes it too — so a code can never
/// be guessed at more than `max_attempts` times no matter how requests race.
pub async fn verify_code(
    conn: &mut PgConnection,
    challenge_id: Uuid,
    purpose: Purpose,
    code_hash: &[u8],
    max_attempts: i32,
) -> Result<CodeOutcome, AppError> {
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        "UPDATE auth_tokens \
            SET attempts = attempts + 1, \
                consumed_at = CASE \
                    WHEN token_hash = $2 OR attempts + 1 >= $3 THEN now() \
                    ELSE NULL \
                END, \
                updated_at = now() \
          WHERE id = $1 AND purpose = $4 \
            AND consumed_at IS NULL AND expires_at > now() \
      RETURNING user_id, (token_hash = $2)",
    )
    .bind(challenge_id)
    .bind(code_hash)
    .bind(max_attempts)
    .bind(purpose.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(match row {
        Some((user_id, true)) => CodeOutcome::Matched { user_id },
        Some((_, false)) => CodeOutcome::Wrong,
        None => CodeOutcome::Gone,
    })
}

/// The account behind an open challenge, for re-sending its code.
pub async fn challenge_user(
    conn: &mut PgConnection,
    challenge_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    sqlx::query_scalar(
        "SELECT user_id FROM auth_tokens \
          WHERE id = $1 AND purpose = 'login_code' \
            AND consumed_at IS NULL AND expires_at > now()",
    )
    .bind(challenge_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// The account behind an open link-shaped token, without consuming it.
///
/// Used to validate the rest of a request (password policy, account status)
/// before the token is spent — a reset refused for a weak new password must
/// leave the link usable, or every typo costs a fresh email.
pub async fn peek_link(
    conn: &mut PgConnection,
    token_hash: &[u8],
    purpose: Purpose,
) -> Result<Option<Uuid>, AppError> {
    sqlx::query_scalar(
        "SELECT user_id FROM auth_tokens \
          WHERE token_hash = $1 AND purpose = $2 \
            AND consumed_at IS NULL AND expires_at > now()",
    )
    .bind(token_hash)
    .bind(purpose.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Consume a link-shaped token by its digest. Returns the account it belongs
/// to, exactly once: the UPDATE is the read, so two racing confirmations
/// cannot both succeed.
pub async fn consume_link(
    conn: &mut PgConnection,
    token_hash: &[u8],
    purpose: Purpose,
) -> Result<Option<Uuid>, AppError> {
    sqlx::query_scalar(
        "UPDATE auth_tokens SET consumed_at = now(), updated_at = now() \
          WHERE token_hash = $1 AND purpose = $2 \
            AND consumed_at IS NULL AND expires_at > now() \
      RETURNING user_id",
    )
    .bind(token_hash)
    .bind(purpose.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Delete tokens that expired or were consumed more than `grace_days` ago.
pub async fn prune_expired(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    grace_days: i64,
    batch: i64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "DELETE FROM auth_tokens WHERE ctid IN ( \
             SELECT ctid FROM auth_tokens \
              WHERE expires_at < $1 - make_interval(days => $2) \
                 OR (consumed_at IS NOT NULL AND consumed_at < $1 - make_interval(days => $2)) \
              LIMIT $3 \
         )",
    )
    .bind(now)
    .bind(grace_days as i32)
    .bind(batch)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected())
}
