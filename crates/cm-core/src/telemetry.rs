//! Tracing setup.
//!
//! One JSON object per line in staging and production, so journald and any log
//! shipper downstream can parse it without a regex; human-readable output in
//! development. Both carry the request id emitted by the API layer.

use crate::config::{Environment, LogFormat};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("a tracing subscriber is already installed for this process")]
    AlreadyInitialised,
}

/// Install the global subscriber. Call once, early, before anything logs.
pub fn init(environment: Environment, format: LogFormat) -> Result<(), TelemetryError> {
    // RUST_LOG wins when set, so an operator can raise the level on a running
    // box without a code change.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_filter(environment)));

    let registry = tracing_subscriber::registry().with(filter);

    let result = match format {
        LogFormat::Json => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(true)
                    .with_span_list(false),
            )
            .try_init(),
        LogFormat::Pretty => registry
            .with(tracing_subscriber::fmt::layer().with_target(true))
            .try_init(),
    };

    result.map_err(|_| TelemetryError::AlreadyInitialised)
}

/// Quieter in production: `debug` on a busy box is a disk-space incident.
fn default_filter(environment: Environment) -> &'static str {
    match environment {
        Environment::Development => {
            "info,cm_api=debug,cm_db=debug,cm_server=debug,tower_http=debug"
        }
        Environment::Staging | Environment::Production => "info,sqlx=warn",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_is_more_verbose_than_production() {
        assert!(default_filter(Environment::Development).contains("debug"));
        assert!(!default_filter(Environment::Production).contains("debug"));
    }

    #[test]
    fn every_default_filter_parses() {
        for environment in [
            Environment::Development,
            Environment::Staging,
            Environment::Production,
        ] {
            EnvFilter::try_new(default_filter(environment))
                .unwrap_or_else(|e| panic!("{environment} filter is not parseable: {e}"));
        }
    }
}
