//! The geocoding queue.
//!
//! Workers claim rows with `FOR UPDATE SKIP LOCKED`, so running several is safe
//! and none of them blocks another. Everything is bounded: a claim takes at
//! most `limit` rows, failures back off, and attempts are capped.

use chrono::{DateTime, Utc};
use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    InProgress,
    Succeeded,
    Failed,
    Skipped,
}

impl JobStatus {
    pub const ALL: [Self; 5] = [
        Self::Queued,
        Self::InProgress,
        Self::Succeeded,
        Self::Failed,
        Self::Skipped,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: Uuid,
    pub contractor_id: Uuid,
    pub attempts: i32,
}

/// Queue a contractor for geocoding.
///
/// A partial unique index allows at most one open job per contractor, so
/// re-queuing an address already waiting is a no-op rather than a second row.
/// Returns whether a new job was created.
pub async fn enqueue(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    address_hash: &[u8],
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "INSERT INTO geocode_queue (id, contractor_id, address_hash) \
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(new_id())
    .bind(contractor_id)
    .bind(address_hash)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

/// Claim up to `limit` jobs that are due.
///
/// `SKIP LOCKED` is what makes a second worker useful instead of merely
/// blocked. The rows are marked in the same transaction, so a crash between
/// claiming and working leaves them `in_progress` — recovered by `requeue_stalled`.
pub async fn claim(
    conn: &mut PgConnection,
    worker: &str,
    limit: i64,
) -> Result<Vec<Job>, AppError> {
    sqlx::query_as(
        "WITH due AS ( \
             SELECT id FROM geocode_queue \
              WHERE status = 'queued' AND next_attempt_at <= now() \
              ORDER BY next_attempt_at \
              FOR UPDATE SKIP LOCKED \
              LIMIT $2 \
         ) \
         UPDATE geocode_queue q \
            SET status = 'in_progress', locked_at = now(), locked_by = $1, updated_at = now() \
           FROM due \
          WHERE q.id = due.id \
      RETURNING q.id, q.contractor_id, q.attempts",
    )
    .bind(worker)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn mark_succeeded(
    conn: &mut PgConnection,
    job_id: Uuid,
    provider: &str,
    response: Option<&serde_json::Value>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE geocode_queue \
            SET status = 'succeeded', provider = $2, provider_response = $3, \
                locked_at = NULL, locked_by = NULL, last_error = NULL, updated_at = now() \
          WHERE id = $1",
    )
    .bind(job_id)
    .bind(provider)
    .bind(response)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Record a failure and schedule a retry, or give up once attempts run out.
pub async fn mark_failed(
    conn: &mut PgConnection,
    job_id: Uuid,
    error: &str,
    backoff_secs: i64,
    max_attempts: i32,
) -> Result<JobStatus, AppError> {
    // Truncated so a verbose provider error cannot exceed the column bound.
    let error: String = error.chars().take(2000).collect();

    let status: String = sqlx::query_scalar(
        "UPDATE geocode_queue \
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
    .bind(job_id)
    .bind(&error)
    .bind(backoff_secs as f64)
    .bind(max_attempts)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(JobStatus::ALL
        .into_iter()
        .find(|candidate| candidate.as_str() == status)
        .unwrap_or(JobStatus::Queued))
}

/// A job with nothing to geocode — no address on the licence record.
pub async fn mark_skipped(
    conn: &mut PgConnection,
    job_id: Uuid,
    reason: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE geocode_queue \
            SET status = 'skipped', last_error = $2, locked_at = NULL, locked_by = NULL, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(job_id)
    .bind(reason)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Return jobs abandoned by a worker that died mid-flight.
///
/// Without this, a crash leaks queue capacity permanently: the rows stay
/// `in_progress` and no worker will ever claim them again.
pub async fn requeue_stalled(
    conn: &mut PgConnection,
    stale_after_secs: i64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "UPDATE geocode_queue \
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

/// Queue depth by status, for the operational counter.
pub async fn depth(conn: &mut PgConnection) -> Result<Vec<(String, i64)>, AppError> {
    sqlx::query_as("SELECT status, count(*) FROM geocode_queue GROUP BY status ORDER BY status")
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// How many contractors have no published point. An operational signal: these
/// are invisible to distance search, and silently so.
pub async fn unlocated_contractor_count(conn: &mut PgConnection) -> Result<i64, AppError> {
    sqlx::query_scalar("SELECT count(*) FROM contractors WHERE public_point IS NULL")
        .fetch_one(&mut *conn)
        .await
        .map_err(AppError::internal)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Job {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            contractor_id: row.try_get("contractor_id")?,
            attempts: row.try_get("attempts")?,
        })
    }
}

/// When a job was last touched, for tests and diagnostics.
pub async fn status_of(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Option<(String, i32, DateTime<Utc>)>, AppError> {
    sqlx::query_as(
        "SELECT status, attempts, next_attempt_at FROM geocode_queue \
          WHERE contractor_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(contractor_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}
