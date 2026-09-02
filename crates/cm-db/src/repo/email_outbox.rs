//! The email outbox.
//!
//! Mail is enqueued in the transaction that creates the reason for it and
//! delivered by the mail worker, so a crashed request or a provider outage
//! delays mail rather than losing it. Workers claim rows with
//! `FOR UPDATE SKIP LOCKED`; everything is bounded the same way the geocode
//! queue is.

use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    LoginCode,
    PasswordReset,
    JobAlert,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::LoginCode, Self::PasswordReset, Self::JobAlert];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoginCode => "login_code",
            Self::PasswordReset => "password_reset",
            Self::JobAlert => "job_alert",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageStatus {
    Queued,
    InProgress,
    Sent,
    Failed,
}

impl MessageStatus {
    pub const ALL: [Self; 4] = [Self::Queued, Self::InProgress, Self::Sent, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }
}

/// A message to enqueue, fully rendered. Bodies are built at enqueue time
/// because the values they carry — a code, a single-use link — exist only in
/// the transaction that issues them.
#[derive(Debug, Clone)]
pub struct NewEmail {
    pub user_id: Uuid,
    pub recipient: String,
    pub kind: Kind,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub unsubscribe_url: Option<String>,
}

/// A claimed message, everything the worker needs to post it.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub id: Uuid,
    pub recipient: String,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub unsubscribe_url: Option<String>,
    pub attempts: i32,
}

/// Queue one message. Returns its id, which the worker later hands to the
/// provider as an idempotency key.
pub async fn enqueue(conn: &mut PgConnection, email: &NewEmail) -> Result<Uuid, AppError> {
    let id = new_id();
    sqlx::query(
        "INSERT INTO email_outbox \
             (id, user_id, recipient, kind, subject, body_text, body_html, unsubscribe_url) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(email.user_id)
    .bind(&email.recipient)
    .bind(email.kind.as_str())
    .bind(&email.subject)
    .bind(&email.body_text)
    .bind(&email.body_html)
    .bind(&email.unsubscribe_url)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(id)
}

/// Claim up to `limit` due messages.
///
/// `SKIP LOCKED` makes a second worker useful instead of blocked. Rows are
/// marked in the same transaction, so a crash between claiming and sending
/// leaves them `in_progress` — recovered by `requeue_stalled`.
pub async fn claim(
    conn: &mut PgConnection,
    worker: &str,
    limit: i64,
) -> Result<Vec<Claimed>, AppError> {
    sqlx::query_as(
        "WITH due AS ( \
             SELECT id FROM email_outbox \
              WHERE status = 'queued' AND next_attempt_at <= now() \
              ORDER BY next_attempt_at \
              FOR UPDATE SKIP LOCKED \
              LIMIT $2 \
         ) \
         UPDATE email_outbox m \
            SET status = 'in_progress', locked_at = now(), locked_by = $1, updated_at = now() \
           FROM due \
          WHERE m.id = due.id \
      RETURNING m.id, m.recipient, m.subject, m.body_text, m.body_html, \
                m.unsubscribe_url, m.attempts",
    )
    .bind(worker)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn mark_sent(
    conn: &mut PgConnection,
    message_id: Uuid,
    provider_message_id: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE email_outbox \
            SET status = 'sent', provider_message_id = $2, \
                locked_at = NULL, locked_by = NULL, last_error = NULL, updated_at = now() \
          WHERE id = $1",
    )
    .bind(message_id)
    .bind(provider_message_id)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Record a failure and schedule a retry, or give up once attempts run out.
pub async fn mark_failed(
    conn: &mut PgConnection,
    message_id: Uuid,
    error: &str,
    backoff_secs: i64,
    max_attempts: i32,
) -> Result<MessageStatus, AppError> {
    // Truncated so a verbose provider error cannot exceed the column bound.
    let error: String = error.chars().take(2000).collect();

    let status: String = sqlx::query_scalar(
        "UPDATE email_outbox \
            SET attempts = attempts + 1, \
                last_error = $2, \
                locked_at = NULL, \
                locked_by = NULL, \
                status = CASE WHEN attempts + 1 >= $4 THEN 'failed' ELSE 'queued' END, \
                next_attempt_at = now() + make_interval(secs => $3), \
                updated_at = now() \
          WHERE id = $1 \
      RETURNING status",
    )
    .bind(message_id)
    .bind(&error)
    .bind(backoff_secs as f64)
    .bind(max_attempts)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(MessageStatus::ALL
        .into_iter()
        .find(|candidate| candidate.as_str() == status)
        .unwrap_or(MessageStatus::Queued))
}

/// Return messages abandoned by a worker that died mid-flight.
pub async fn requeue_stalled(
    conn: &mut PgConnection,
    stale_after_secs: i64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "UPDATE email_outbox \
            SET status = 'queued', locked_at = NULL, locked_by = NULL, updated_at = now() \
          WHERE status = 'in_progress' \
            AND locked_at < now() - make_interval(secs => $1)",
    )
    .bind(stale_after_secs as f64)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected())
}

/// Outbox depth by status, for the operational counter and the runbook's
/// "is the provider broken" query.
pub async fn depth(conn: &mut PgConnection) -> Result<Vec<(String, i64)>, AppError> {
    sqlx::query_as("SELECT status, count(*) FROM email_outbox GROUP BY status ORDER BY status")
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// A message's state, for tests and diagnostics.
pub async fn status_of(
    conn: &mut PgConnection,
    message_id: Uuid,
) -> Result<Option<(String, i32, Option<String>)>, AppError> {
    sqlx::query_as("SELECT status, attempts, last_error FROM email_outbox WHERE id = $1")
        .bind(message_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Claimed {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            recipient: row.try_get("recipient")?,
            subject: row.try_get("subject")?,
            body_text: row.try_get("body_text")?,
            body_html: row.try_get("body_html")?,
            unsubscribe_url: row.try_get("unsubscribe_url")?,
            attempts: row.try_get("attempts")?,
        })
    }
}
