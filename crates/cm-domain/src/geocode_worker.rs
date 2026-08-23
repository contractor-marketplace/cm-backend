//! The geocoding worker.
//!
//! Three properties this is built around:
//!
//! * **No pooled connection is held across a provider call.** The same rule as
//!   password hashing: a connection held across a 20-second HTTP timeout would
//!   let a slow provider check out the pool and starve the API.
//! * **Bounded everywhere.** A claim takes at most `batch` rows, failures back
//!   off, attempts are capped, and the provider is called no faster than
//!   `rate_per_second`.
//! * **Crash-safe.** Jobs are marked `in_progress` when claimed; a worker that
//!   dies leaves them there, and `requeue_stalled` returns them. Without that,
//!   every crash would leak queue capacity permanently.

use cm_core::AppError;
use cm_db::repo::{contractors, geocode};
use cm_db::PgPool;
use std::sync::Arc;
use std::time::Duration;

use crate::geocoder::{Geocoder, Located};

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Jobs claimed per pass.
    pub batch: i64,
    /// Attempts before a job is given up on.
    pub max_attempts: i32,
    /// Ceiling on provider calls per second.
    pub rate_per_second: f64,
    /// Identifies this worker in `locked_by`, for diagnosing a stall.
    pub worker_id: String,
    /// How long a claimed job may sit before it is assumed abandoned.
    pub stale_after_secs: i64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            batch: 25,
            max_attempts: 5,
            // The Census service is free and shared; this is deliberately
            // gentle rather than as fast as it will go.
            rate_per_second: 2.0,
            worker_id: "geocode-worker".to_owned(),
            stale_after_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub claimed: u64,
    pub located: u64,
    pub not_found: u64,
    pub failed: u64,
    pub skipped: u64,
    pub requeued: u64,
}

/// Exponential backoff with a ceiling, so a provider outage does not turn into
/// a tight retry loop.
fn backoff_secs(attempts: i32) -> i64 {
    const BASE: i64 = 30;
    const CEILING: i64 = 3600;
    BASE.saturating_mul(1i64 << attempts.clamp(0, 10))
        .min(CEILING)
}

/// One pass over the queue. Returns when the claimed batch is done.
pub async fn run_once(
    pool: &PgPool,
    geocoder: &Arc<dyn Geocoder>,
    config: &WorkerConfig,
) -> Result<Stats, AppError> {
    let mut stats = Stats::default();

    // Recover anything a dead worker left claimed.
    {
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        stats.requeued = geocode::requeue_stalled(&mut conn, config.stale_after_secs).await?;
    }

    let jobs = {
        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let jobs = geocode::claim(&mut tx, &config.worker_id, config.batch).await?;
        tx.commit().await.map_err(AppError::internal)?;
        jobs
    };
    stats.claimed = jobs.len() as u64;

    let min_interval = Duration::from_secs_f64(1.0 / config.rate_per_second.max(0.01));

    for job in jobs {
        // Read the address, then let the connection go before the network call.
        let address = {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            contractors::geocodable_address(&mut conn, job.contractor_id).await?
        };

        let Some(address) = address else {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            geocode::mark_skipped(&mut conn, job.id, "no address on the licence record").await?;
            stats.skipped += 1;
            continue;
        };

        // Nothing from the pool is held here.
        let located = geocoder.locate(address).await;

        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        match located {
            Ok(Located::Found { coordinates, raw }) => {
                let mut tx = pool.begin().await.map_err(AppError::internal)?;
                let precision = crate::location::apply_geocode(
                    &mut tx,
                    job.contractor_id,
                    coordinates.lat,
                    coordinates.lon,
                )
                .await?;
                geocode::mark_succeeded(&mut tx, job.id, geocoder.name(), Some(&raw)).await?;
                tx.commit().await.map_err(AppError::internal)?;

                tracing::debug!(
                    contractor_id = %job.contractor_id,
                    precision = precision.as_str(),
                    "located a contractor"
                );
                stats.located += 1;
            }
            Ok(Located::NotFound) => {
                // Nothing to retry: the provider answered and had no match. The
                // contractor keeps its ZIP centroid and stays searchable.
                geocode::mark_skipped(&mut conn, job.id, "no match from the geocoder").await?;
                stats.not_found += 1;
            }
            Err(error) => {
                let status = geocode::mark_failed(
                    &mut conn,
                    job.id,
                    &error.to_string(),
                    backoff_secs(job.attempts),
                    config.max_attempts,
                )
                .await?;
                tracing::warn!(
                    contractor_id = %job.contractor_id,
                    attempts = job.attempts + 1,
                    status = status.as_str(),
                    %error,
                    "geocoding attempt failed"
                );
                stats.failed += 1;
            }
        }

        drop(conn);
        tokio::time::sleep(min_interval).await;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        assert_eq!(backoff_secs(0), 30);
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(10), 3600, "capped");
        assert_eq!(
            backoff_secs(1_000),
            3600,
            "still capped, and does not overflow"
        );
        assert_eq!(backoff_secs(-1), 30, "a nonsensical count does not panic");
    }

    #[test]
    fn the_default_rate_is_gentle_and_bounded() {
        let config = WorkerConfig::default();
        assert!(
            config.rate_per_second <= 5.0,
            "the Census service is shared"
        );
        assert!(config.batch > 0 && config.batch <= 100);
        assert!(config.max_attempts > 0);
        assert!(config.stale_after_secs > 0);
    }
}
