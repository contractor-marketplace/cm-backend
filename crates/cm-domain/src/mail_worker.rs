//! The mail worker.
//!
//! Drains the email outbox into a provider. Built on the same three properties
//! as the geocoding worker:
//!
//! * **No pooled connection is held across a provider call.** A connection
//!   held across a 30-second HTTP timeout would let a slow provider check out
//!   the pool and starve the API.
//! * **Bounded everywhere.** A claim takes at most `batch` rows, failures back
//!   off, attempts are capped, and the provider is called no faster than
//!   `rate_per_second`.
//! * **Crash-safe.** Rows are `in_progress` while claimed; a dead worker's
//!   claims are returned by `requeue_stalled`, and the provider idempotency
//!   key (the row id) means a resend after an ambiguous failure cannot become
//!   a second email.

use cm_core::AppError;
use cm_db::repo::email_outbox;
use cm_db::PgPool;
use std::time::Duration;

use crate::mailer::{Email, Mailer};

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Messages claimed per pass.
    pub batch: i64,
    /// Attempts before a message is given up on.
    pub max_attempts: i32,
    /// Ceiling on provider calls per second. Resend's default rate limit is
    /// 2/s; staying at it beats discovering it as failures.
    pub rate_per_second: f64,
    /// Identifies this worker in `locked_by`, for diagnosing a stall.
    pub worker_id: String,
    /// How long a claimed message may sit before it is assumed abandoned.
    pub stale_after_secs: i64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            batch: 25,
            max_attempts: 8,
            rate_per_second: 2.0,
            worker_id: "mail-worker".to_owned(),
            stale_after_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub requeued: u64,
    pub claimed: u64,
    pub sent: u64,
    pub failed: u64,
}

/// Exponential backoff with a ceiling, so a provider outage does not turn into
/// a tight retry loop.
fn backoff_secs(attempts: i32) -> i64 {
    const BASE: i64 = 30;
    const CEILING: i64 = 3600;
    BASE.saturating_mul(1i64 << attempts.clamp(0, 10))
        .min(CEILING)
}

/// One pass over the outbox. Returns when the claimed batch is done.
pub async fn run_once(
    pool: &PgPool,
    mailer: &Mailer,
    config: &WorkerConfig,
) -> Result<Stats, AppError> {
    let mut stats = Stats::default();

    // Recover anything a dead worker left claimed.
    {
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        stats.requeued = email_outbox::requeue_stalled(&mut conn, config.stale_after_secs).await?;
    }

    let messages = {
        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let messages = email_outbox::claim(&mut tx, &config.worker_id, config.batch).await?;
        tx.commit().await.map_err(AppError::internal)?;
        messages
    };
    stats.claimed = messages.len() as u64;

    let min_interval = Duration::from_secs_f64(1.0 / config.rate_per_second.max(0.01));

    for message in messages {
        let email = Email {
            id: message.id,
            to: message.recipient,
            subject: message.subject,
            body_text: message.body_text,
            body_html: message.body_html,
            unsubscribe_url: message.unsubscribe_url,
        };

        // Nothing from the pool is held here.
        let outcome = mailer.send(email).await;

        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        match outcome {
            Ok(provider_message_id) => {
                email_outbox::mark_sent(&mut conn, message.id, provider_message_id.as_deref())
                    .await?;
                stats.sent += 1;
            }
            Err(error) => {
                let status = email_outbox::mark_failed(
                    &mut conn,
                    message.id,
                    &error.to_string(),
                    backoff_secs(message.attempts),
                    config.max_attempts,
                )
                .await?;
                tracing::warn!(
                    message_id = %message.id,
                    attempts = message.attempts + 1,
                    status = status.as_str(),
                    %error,
                    "sending an email failed"
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
        assert_eq!(backoff_secs(10), 3600, "capped");
        assert_eq!(backoff_secs(1_000), 3600, "still capped, no overflow");
        assert_eq!(backoff_secs(-1), 30, "a nonsensical count does not panic");
    }

    #[test]
    fn the_default_rate_respects_the_provider_limit() {
        let config = WorkerConfig::default();
        assert!(config.rate_per_second <= 2.0, "Resend's default is 2/s");
        assert!(config.batch > 0 && config.batch <= 100);
        assert!(config.max_attempts > 0);
        assert!(config.stale_after_secs > 0);
    }
}
