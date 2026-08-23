//! Bounded retention for the tables that would otherwise grow forever.
//!
//! Three tables accumulate rows that nothing ever needs again:
//!
//! * `sessions` — a revoked or long-expired session can never be used, but the
//!   row stays. On a busy service this is the fastest-growing table in the
//!   schema, and nothing was deleting it.
//! * `geocode_queue` — jobs that succeeded, were skipped, or ran out of
//!   attempts are terminal. The queue is not a log.
//! * `rate_limit_counters` — swept separately, on a shorter cycle.
//!
//! `audit_log` is deliberately **not** pruned by default: deleting an audit
//! trail is a policy decision with legal weight, not a housekeeping one. A
//! retention period can be passed explicitly.
//!
//! Every delete here is bounded by a batch size. An unbounded `DELETE` on a
//! table that has been left to grow is a long transaction holding locks, which
//! is the wrong thing to run on a box that is also serving requests.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use sqlx::PgConnection;

/// Rows removed per statement.
pub const BATCH: i64 = 5_000;
/// A single pass never runs longer than this many batches, so a very stale
/// table is caught up over several runs rather than in one long lock.
pub const MAX_BATCHES: usize = 40;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pruned {
    pub sessions: u64,
    pub geocode_jobs: u64,
    pub rate_limit_windows: u64,
    pub audit_rows: u64,
}

/// Delete sessions that ended more than `grace_days` ago.
///
/// Both conditions matter: a revoked session is finished, and an expired one
/// cannot be revived. The grace period keeps recent history available for
/// answering "was I logged out, and when".
pub async fn prune_sessions(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    grace_days: i64,
    batch: i64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "DELETE FROM sessions WHERE ctid IN ( \
             SELECT ctid FROM sessions \
              WHERE absolute_expires_at < $1 - make_interval(days => $2) \
                 OR (revoked_at IS NOT NULL AND revoked_at < $1 - make_interval(days => $2)) \
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

/// Delete geocode jobs that reached a terminal state more than `grace_days` ago.
pub async fn prune_geocode_jobs(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    grace_days: i64,
    batch: i64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "DELETE FROM geocode_queue WHERE ctid IN ( \
             SELECT ctid FROM geocode_queue \
              WHERE status IN ('succeeded', 'skipped', 'failed') \
                AND updated_at < $1 - make_interval(days => $2) \
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

/// Delete audit rows older than `days`. Only ever called when an operator asks.
pub async fn prune_audit(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    days: i64,
    batch: i64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "DELETE FROM audit_log WHERE ctid IN ( \
             SELECT ctid FROM audit_log \
              WHERE created_at < $1 - make_interval(days => $2) \
              LIMIT $3 \
         )",
    )
    .bind(now)
    .bind(days as i32)
    .bind(batch)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected())
}

/// Row counts for the tables that grow, so the numbers are observable before
/// they become a problem.
///
/// Exact counts, not `pg_stat_user_tables.n_live_tup`. The statistics view is
/// an estimate that autovacuum updates on its own schedule, so a report built
/// from it shows the same numbers before and after a prune and reads as though
/// nothing was deleted.
pub async fn growth_report(conn: &mut PgConnection) -> Result<Vec<(String, i64)>, AppError> {
    sqlx::query_as(
        "SELECT t.name::text, t.rows FROM ( \
             SELECT 'sessions' AS name, count(*) AS rows FROM sessions \
             UNION ALL SELECT 'audit_log', count(*) FROM audit_log \
             UNION ALL SELECT 'rate_limit_counters', count(*) FROM rate_limit_counters \
             UNION ALL SELECT 'geocode_queue', count(*) FROM geocode_queue \
             UNION ALL SELECT 'messages', count(*) FROM messages \
             UNION ALL SELECT 'license_record_versions', count(*) FROM license_record_versions \
         ) t ORDER BY t.rows DESC, t.name",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}
