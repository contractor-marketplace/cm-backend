//! Durable fixed-window rate limiting.
//!
//! Bucket keys arrive already hashed: this module never sees an IP address or a
//! user id in the clear, which is why the table can be read by anyone debugging
//! a limit without exposing who was limited.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cm_core::AppError;
use sqlx::PgConnection;

/// The outcome of counting one request against a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub count: i32,
    pub limit: i32,
    pub window_start: DateTime<Utc>,
    pub window: ChronoDuration,
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        self.count <= self.limit
    }

    /// How long until this bucket resets.
    pub fn retry_after(&self, now: DateTime<Utc>) -> std::time::Duration {
        let resets_at = self.window_start + self.window;
        (resets_at - now)
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(1))
            .max(std::time::Duration::from_secs(1))
    }
}

/// Count one request against a bucket and report where it stands.
///
/// A single upsert: concurrent requests against one bucket serialise on the
/// primary key rather than racing a read-modify-write, so the count cannot be
/// undercounted by interleaving.
pub async fn hit(
    conn: &mut PgConnection,
    bucket_hash: &[u8],
    limit: i32,
    window: ChronoDuration,
    now: DateTime<Utc>,
) -> Result<Decision, AppError> {
    let window_secs = window.num_seconds().max(1);
    // Fixed windows aligned to the epoch, so every process agrees on the
    // boundary without coordinating.
    let window_start =
        DateTime::from_timestamp(now.timestamp() - now.timestamp().rem_euclid(window_secs), 0)
            .ok_or_else(|| AppError::internal("window start is out of range"))?;
    let expires_at = window_start + window;

    let count: i32 = sqlx::query_scalar(
        "INSERT INTO rate_limit_counters (bucket_hash, window_start, count, expires_at) \
         VALUES ($1, $2, 1, $3) \
         ON CONFLICT (bucket_hash, window_start) DO UPDATE \
             SET count = rate_limit_counters.count + 1, updated_at = now() \
         RETURNING count",
    )
    .bind(bucket_hash)
    .bind(window_start)
    .bind(expires_at)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(Decision {
        count,
        limit,
        window_start,
        window,
    })
}

/// Delete elapsed windows, at most `batch` rows per call.
///
/// Bounded on purpose: an unbounded `DELETE` on a table that has been left to
/// grow is a long transaction holding locks, which is exactly the wrong thing
/// to run on a single box under load. The sweeper calls this repeatedly and
/// stops when a call deletes fewer rows than it asked for.
pub async fn sweep_expired(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    batch: i64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "DELETE FROM rate_limit_counters \
          WHERE ctid IN ( \
              SELECT ctid FROM rate_limit_counters WHERE expires_at <= $1 LIMIT $2 \
          )",
    )
    .bind(now)
    .bind(batch)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected())
}
