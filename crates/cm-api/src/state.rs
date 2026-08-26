//! Shared handler state.

use cm_auth::AuthService;
use cm_core::{AppError, Config, Environment};
use cm_db::PgPool;
use cm_storage::Store;
use std::sync::Arc;

/// Identity of the running binary, reported by `/version`.
///
/// The git SHA is injected at build time; an unstamped build says so rather
/// than claiming a version it cannot prove.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    pub version: &'static str,
    pub git_sha: &'static str,
}

impl BuildInfo {
    pub const fn from_env() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            git_sha: match option_env!("CM_GIT_SHA") {
                Some(sha) => sha,
                None => "unstamped",
            },
        }
    }
}

impl Default for BuildInfo {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub environment: Environment,
    pub build: BuildInfo,
    /// Behind an `Arc` because axum clones the state for every request, and the
    /// service holds a pepper and a semaphore that must not be duplicated.
    pub auth: Arc<AuthService>,
    /// Whether `X-Forwarded-For` may be believed when identifying a client.
    pub trust_proxy_headers: bool,
    /// Where job photos go. In-memory unless a bucket is configured, which
    /// production refuses to start without.
    pub store: Store,
}

impl AppState {
    pub fn new(pool: PgPool, config: &Config) -> Result<Self, AppError> {
        Ok(Self {
            pool,
            environment: config.environment,
            build: BuildInfo::from_env(),
            auth: Arc::new(AuthService::new(&config.auth, config.site_origin.clone())?),
            trust_proxy_headers: config.auth.trust_proxy_headers,
            store: match &config.job_photo_bucket {
                Some(bucket) => Store::Gcs(cm_storage::gcs::Bucket::new(bucket)?),
                None => Store::memory(),
            },
        })
    }
}
