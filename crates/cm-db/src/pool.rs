//! Connection pool construction and liveness.

use cm_core::{AppError, DatabaseConfig};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use std::str::FromStr;
use std::time::Duration;

/// Connect eagerly, verifying one connection before returning.
///
/// Used by `serve` and `migrate`: failing here means the process exits with a
/// clear message instead of accepting traffic it cannot serve.
pub async fn connect(config: &DatabaseConfig) -> Result<PgPool, AppError> {
    options(config)?
        .connect_with(connect_options(config)?)
        .await
        .map_err(AppError::internal)
}

/// Build a pool without opening a connection.
///
/// Used by the readiness tests, which need a pool that points at a database
/// that is deliberately unreachable, to prove `/healthz` answers anyway.
pub fn connect_lazy(config: &DatabaseConfig) -> Result<PgPool, AppError> {
    Ok(options(config)?.connect_lazy_with(connect_options(config)?))
}

fn options(config: &DatabaseConfig) -> Result<PgPoolOptions, AppError> {
    Ok(PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        // A connection idle for half an hour on a box this size is memory we
        // would rather have back.
        .idle_timeout(Duration::from_secs(30 * 60))
        .max_lifetime(Duration::from_secs(60 * 60))
        // The fuzzy-name threshold is session state that the `<%` operator
        // reads, so it has to be set on every connection rather than once. A
        // connection that misses this searches more strictly than the rest of
        // the pool, which would show up as a business being findable or not
        // depending on which connection served the request.
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query(&format!(
                    "SET pg_trgm.word_similarity_threshold = {}",
                    crate::repo::search::WORD_SIMILARITY_THRESHOLD
                ))
                .execute(conn)
                .await?;
                Ok(())
            })
        }))
}

fn connect_options(config: &DatabaseConfig) -> Result<PgConnectOptions, AppError> {
    let options = PgConnectOptions::from_str(config.url.expose())
        .map_err(|e| AppError::invalid(format!("DATABASE_URL could not be parsed: {e}")))?
        // sqlx logs every statement at INFO by default, which at production
        // volume is both noise and a way for parameters to reach a log file.
        .log_statements(tracing::log::LevelFilter::Debug)
        .log_slow_statements(tracing::log::LevelFilter::Warn, Duration::from_millis(250));

    Ok(options)
}

/// Cheapest possible round trip, for readiness checks.
pub async fn ping(pool: &PgPool) -> Result<(), AppError> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(AppError::internal)
}
