//! Scheduled housekeeping.
//!
//! Run from a timer, not from a request: a delete on the latency path of
//! whichever unlucky caller triggered it is how a maintenance job becomes a
//! user-visible stall.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use cm_db::repo::maintenance::{self, Pruned, BATCH, MAX_BATCHES};
use cm_db::PgPool;

/// How long a finished session or geocode job is kept before deletion.
pub const DEFAULT_GRACE_DAYS: i64 = 30;

/// Delete what nothing needs any more, in bounded batches.
///
/// `audit_days` is `None` unless an operator asks: deleting an audit trail
/// is a policy decision, not housekeeping.
pub async fn prune(
    pool: &PgPool,
    now: DateTime<Utc>,
    grace_days: i64,
    audit_days: Option<i64>,
) -> Result<Pruned, AppError> {
    let sessions = drain(pool, |conn| {
        Box::pin(maintenance::prune_sessions(conn, now, grace_days, BATCH))
    })
    .await?;

    let geocode_jobs = drain(pool, |conn| {
        Box::pin(maintenance::prune_geocode_jobs(
            conn, now, grace_days, BATCH,
        ))
    })
    .await?;

    let rate_limit_windows = cm_auth::ratelimit::sweep(pool, now).await?;

    let audit_rows = match audit_days {
        Some(days) => {
            drain(pool, |conn| {
                Box::pin(maintenance::prune_audit(conn, now, days, BATCH))
            })
            .await?
        }
        None => 0,
    };

    Ok(Pruned {
        sessions,
        geocode_jobs,
        rate_limit_windows,
        audit_rows,
    })
}

/// Repeat a bounded delete until it stops finding rows, or the pass runs
/// out of batches. Each batch is its own short transaction.
async fn drain<F>(pool: &PgPool, mut step: F) -> Result<u64, AppError>
where
    F: for<'c> FnMut(
        &'c mut sqlx::PgConnection,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<u64, AppError>> + Send + 'c>,
    >,
{
    let mut removed = 0;
    for _ in 0..MAX_BATCHES {
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        let batch = step(&mut conn).await?;
        removed += batch;
        if batch < BATCH as u64 {
            break;
        }
    }
    Ok(removed)
}

/// Row counts for the tables that grow.
pub async fn growth_report(pool: &PgPool) -> Result<Vec<(String, i64)>, AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    maintenance::growth_report(&mut conn).await
}
