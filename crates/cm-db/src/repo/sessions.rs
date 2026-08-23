//! Sessions.
//!
//! The raw token never reaches this module. Callers hash it first, and only the
//! digest is stored or matched, so a database dump yields nothing replayable.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

/// Why a session was ended. Mirrors the `sessions.revoked_reason` CHECK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevocationReason {
    Logout,
    LogoutAll,
    PasswordChange,
    Rotation,
    Admin,
}

impl RevocationReason {
    pub const ALL: [Self; 5] = [
        Self::Logout,
        Self::LogoutAll,
        Self::PasswordChange,
        Self::Rotation,
        Self::Admin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Logout => "logout",
            Self::LogoutAll => "logout_all",
            Self::PasswordChange => "password_change",
            Self::Rotation => "rotation",
            Self::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub idle_expires_at: DateTime<Utc>,
    pub absolute_expires_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Uuid,
    user_id: Uuid,
    idle_expires_at: DateTime<Utc>,
    absolute_expires_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
}

impl From<SessionRow> for Session {
    fn from(row: SessionRow) -> Self {
        Self {
            id: row.id,
            user_id: row.user_id,
            idle_expires_at: row.idle_expires_at,
            absolute_expires_at: row.absolute_expires_at,
            last_seen_at: row.last_seen_at,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn insert(
    conn: &mut PgConnection,
    id: Uuid,
    user_id: Uuid,
    token_hash: &[u8],
    idle_expires_at: DateTime<Utc>,
    absolute_expires_at: DateTime<Utc>,
    ip_hash: Option<&[u8]>,
    user_agent: Option<&str>,
) -> Result<Session, AppError> {
    let row: SessionRow = sqlx::query_as(
        "INSERT INTO sessions \
             (id, user_id, token_hash, idle_expires_at, absolute_expires_at, ip_hash, user_agent) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING id, user_id, idle_expires_at, absolute_expires_at, last_seen_at",
    )
    .bind(id)
    .bind(user_id)
    .bind(token_hash)
    .bind(idle_expires_at)
    .bind(absolute_expires_at)
    .bind(ip_hash)
    .bind(user_agent)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(row.into())
}

/// Resolve a token digest to a live session.
///
/// Revoked and expired sessions are indistinguishable from absent ones here on
/// purpose: the caller has one answer to give either way.
pub async fn find_live(
    conn: &mut PgConnection,
    token_hash: &[u8],
) -> Result<Option<Session>, AppError> {
    let row: Option<SessionRow> = sqlx::query_as(
        "SELECT id, user_id, idle_expires_at, absolute_expires_at, last_seen_at \
           FROM sessions \
          WHERE token_hash = $1 \
            AND revoked_at IS NULL \
            AND idle_expires_at > now() \
            AND absolute_expires_at > now()",
    )
    .bind(token_hash)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(row.map(Into::into))
}

/// Extend the idle window, but only if it has moved meaningfully.
///
/// Without the `last_seen_at` guard this is a write on every authenticated
/// request, which on a single box is the difference between a read-mostly table
/// and a hot one.
pub async fn touch(
    conn: &mut PgConnection,
    session_id: Uuid,
    idle_expires_at: DateTime<Utc>,
    min_interval_secs: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE sessions \
            SET idle_expires_at = LEAST($2, absolute_expires_at), \
                last_seen_at = now(), \
                updated_at = now() \
          WHERE id = $1 \
            AND revoked_at IS NULL \
            AND last_seen_at < now() - make_interval(secs => $3)",
    )
    .bind(session_id)
    .bind(idle_expires_at)
    .bind(min_interval_secs as f64)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Revoke one session. Returns false if it was already revoked.
pub async fn revoke(
    conn: &mut PgConnection,
    session_id: Uuid,
    reason: RevocationReason,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE sessions SET revoked_at = now(), revoked_reason = $2, updated_at = now() \
          WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(session_id)
    .bind(reason.as_str())
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

/// Revoke every live session for an account, optionally sparing one.
///
/// Sparing the caller's own session is what makes a password change rotate
/// rather than log the person out of the browser they are sitting in front of.
pub async fn revoke_all_for_user(
    conn: &mut PgConnection,
    user_id: Uuid,
    reason: RevocationReason,
    except: Option<Uuid>,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "UPDATE sessions SET revoked_at = now(), revoked_reason = $2, updated_at = now() \
          WHERE user_id = $1 AND revoked_at IS NULL AND ($3::uuid IS NULL OR id <> $3)",
    )
    .bind(user_id)
    .bind(reason.as_str())
    .bind(except)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected())
}
