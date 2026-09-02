//! Password credentials and the account-level lockout counter.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

/// Consecutive failures before an account is locked.
pub const MAX_FAILED_ATTEMPTS: i32 = 8;
/// How long a locked account stays locked.
pub const LOCKOUT_MINUTES: i64 = 15;

#[derive(Debug, Clone)]
pub struct PasswordCredential {
    pub user_id: Uuid,
    pub password_hash: String,
    pub failed_attempts: i32,
    pub locked_until: Option<DateTime<Utc>>,
}

impl PasswordCredential {
    pub fn is_locked_at(&self, now: DateTime<Utc>) -> bool {
        self.locked_until.is_some_and(|until| until > now)
    }
}

#[derive(sqlx::FromRow)]
struct CredentialRow {
    user_id: Uuid,
    password_hash: String,
    failed_attempts: i32,
    locked_until: Option<DateTime<Utc>>,
}

impl From<CredentialRow> for PasswordCredential {
    fn from(row: CredentialRow) -> Self {
        Self {
            user_id: row.user_id,
            password_hash: row.password_hash,
            failed_attempts: row.failed_attempts,
            locked_until: row.locked_until,
        }
    }
}

pub async fn insert(
    conn: &mut PgConnection,
    user_id: Uuid,
    password_hash: &str,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO password_credentials (user_id, password_hash) VALUES ($1, $2)")
        .bind(user_id)
        .bind(password_hash)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    Ok(())
}

pub async fn find(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<Option<PasswordCredential>, AppError> {
    let row: Option<CredentialRow> = sqlx::query_as(
        "SELECT user_id, password_hash, failed_attempts, locked_until \
         FROM password_credentials WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(row.map(Into::into))
}

/// Set the password outright, from a completed reset.
///
/// An upsert, not an update: a federated-only account has no credential row,
/// and a reset is exactly how it comes to have one. The failure counter and
/// lock are cleared in the same statement — the proven inbox outranks the
/// guesses that locked the account.
pub async fn set_hash(
    conn: &mut PgConnection,
    user_id: Uuid,
    password_hash: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash) VALUES ($1, $2) \
         ON CONFLICT (user_id) DO UPDATE \
            SET password_hash = $2, failed_attempts = 0, locked_until = NULL, \
                updated_at = now()",
    )
    .bind(user_id)
    .bind(password_hash)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Read the credential and hold a row lock until the transaction ends.
///
/// This is what closes the window between verifying a password and acting on
/// that verification. Without it, a password change can commit in between, and
/// a login would mint a session for a password that is no longer the account's.
/// Callers must be inside a transaction; on a bare connection the lock is
/// released immediately and the guarantee is lost.
pub async fn find_for_update(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<Option<PasswordCredential>, AppError> {
    let row: Option<CredentialRow> = sqlx::query_as(
        "SELECT user_id, password_hash, failed_attempts, locked_until \
         FROM password_credentials WHERE user_id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(row.map(Into::into))
}

/// Record a failed attempt and lock the account if it has now run out.
///
/// One statement, so concurrent failures cannot both read 7 and both write 8:
/// the counter is incremented and tested inside the same update, and the lock
/// is set from the post-increment value.
pub async fn record_failure(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<PasswordCredential, AppError> {
    let row: CredentialRow = sqlx::query_as(
        "UPDATE password_credentials \
            SET failed_attempts = failed_attempts + 1, \
                locked_until = CASE \
                    WHEN failed_attempts + 1 >= $2 THEN now() + make_interval(mins => $3) \
                    ELSE locked_until \
                END, \
                updated_at = now() \
          WHERE user_id = $1 \
      RETURNING user_id, password_hash, failed_attempts, locked_until",
    )
    .bind(user_id)
    .bind(MAX_FAILED_ATTEMPTS)
    .bind(LOCKOUT_MINUTES as i32)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(row.into())
}

/// Clear the failure counter after a successful authentication, or once an
/// expired lock is observed.
pub async fn clear_failures(conn: &mut PgConnection, user_id: Uuid) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE password_credentials \
            SET failed_attempts = 0, locked_until = NULL, updated_at = now() \
          WHERE user_id = $1 AND (failed_attempts <> 0 OR locked_until IS NOT NULL)",
    )
    .bind(user_id)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Replace the stored hash, but only if it is still the one the caller
/// verified. Returns false when it is not.
///
/// A compare-and-swap rather than a blind update: two password changes that
/// both verified the same old password must not both succeed, and the loser has
/// to find out. Also clears the lockout state, since proving knowledge of the
/// current password is at least as strong as waiting a lock out.
pub async fn replace_hash(
    conn: &mut PgConnection,
    user_id: Uuid,
    expected_hash: &str,
    new_hash: &str,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE password_credentials \
            SET password_hash = $3, password_changed_at = now(), \
                failed_attempts = 0, locked_until = NULL, updated_at = now() \
          WHERE user_id = $1 AND password_hash = $2",
    )
    .bind(user_id)
    .bind(expected_hash)
    .bind(new_hash)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

/// Re-hash the same password at stronger parameters.
///
/// Deliberately does not touch `password_changed_at`, `failed_attempts` or
/// `locked_until`: the password did not change, only its storage did, and
/// moving those fields would make an upgrade look like a credential event.
pub async fn upgrade_hash(
    conn: &mut PgConnection,
    user_id: Uuid,
    expected_hash: &str,
    new_hash: &str,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE password_credentials \
            SET password_hash = $3, updated_at = now() \
          WHERE user_id = $1 AND password_hash = $2",
    )
    .bind(user_id)
    .bind(expected_hash)
    .bind(new_hash)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}
