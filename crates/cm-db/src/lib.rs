//! Database access.
//!
//! This is the only crate permitted to contain SQL. Keeping it that way makes
//! "which queries read a restricted column" a grep rather than an audit, which
//! matters later for the location-privacy rule.

pub mod migrate;
pub mod pool;
pub mod repo;

pub use pool::{connect, connect_lazy, ping};
pub use sqlx::PgPool;

/// Shorthand for anything that can execute a statement: a pool, a connection,
/// or a transaction. Repository functions take this so a caller can compose
/// several of them into one transaction — which is how, for example, a login
/// records its session and its audit row atomically.
pub type PgExecutor<'a> = &'a mut sqlx::PgConnection;
