//! Foundations shared by every other crate: configuration, the error taxonomy,
//! identifier generation and telemetry setup.
//!
//! Nothing here performs I/O beyond reading the process environment and
//! installing a tracing subscriber, so it stays usable from unit tests that
//! have no database and no runtime.

pub mod config;
pub mod error;
pub mod id;
pub mod telemetry;

pub use config::{
    AuthConfig, Config, ConfigError, ConfigErrors, DatabaseConfig, Environment, FirebaseConfig,
    LogFormat, MailConfig, Origin, RankingConfig, Secret,
};
pub use error::{AppError, BoxError};
pub use id::new_id;
