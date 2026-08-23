//! Liveness, readiness and build identity.
//!
//! The split matters to the reverse proxy and to any future orchestrator:
//! `/healthz` answers "is this process alive" and must never touch a
//! dependency, or a database blip restarts a perfectly healthy server.
//! `/readyz` answers "should traffic be sent here" and checks everything a
//! request would need.

use crate::state::AppState;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use http::StatusCode;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Liveness {
    status: &'static str,
}

/// Liveness. Deliberately free of I/O.
pub async fn healthz() -> Json<Liveness> {
    Json(Liveness { status: "ok" })
}

#[derive(Debug, Serialize)]
pub struct Readiness {
    status: &'static str,
    checks: ReadinessChecks,
}

#[derive(Debug, Serialize)]
pub struct ReadinessChecks {
    database: CheckResult,
    migrations: MigrationCheck,
}

#[derive(Debug, Serialize)]
pub struct CheckResult {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MigrationCheck {
    status: &'static str,
    embedded: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    applied: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dirty: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// Readiness. 503 until the database answers and the schema matches.
pub async fn readyz(State(state): State<AppState>) -> Response {
    let embedded = cm_db::migrate::embedded_version();

    // The cause is logged, never returned: a connection error string carries
    // the host, the port and the role name.
    if let Err(error) = cm_db::ping(&state.pool).await {
        tracing::warn!(error = %error, "readiness check failed: database unreachable");
        return not_ready(
            CheckResult {
                status: "error",
                detail: Some("database is unreachable".to_owned()),
            },
            MigrationCheck {
                status: "unknown",
                embedded,
                applied: None,
                dirty: Vec::new(),
                detail: Some("not checked: the database is unreachable".to_owned()),
            },
        );
    }

    let status = match cm_db::migrate::status(&state.pool).await {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(error = %error, "readiness check failed: migration status unreadable");
            return not_ready(
                CheckResult {
                    status: "ok",
                    detail: None,
                },
                MigrationCheck {
                    status: "error",
                    embedded,
                    applied: None,
                    dirty: Vec::new(),
                    detail: Some("migration status could not be read".to_owned()),
                },
            );
        }
    };

    let database = CheckResult {
        status: "ok",
        detail: None,
    };

    match status.blocking_reason() {
        // The reason names versions only, so it is safe to return: it tells an
        // operator what to do without describing the deployment.
        Some(detail) => not_ready(
            database,
            MigrationCheck {
                status: "error",
                embedded,
                applied: status.applied,
                dirty: status.dirty,
                detail: Some(detail),
            },
        ),
        None => (
            StatusCode::OK,
            Json(Readiness {
                status: "ready",
                checks: ReadinessChecks {
                    database,
                    migrations: MigrationCheck {
                        status: "ok",
                        embedded,
                        applied: status.applied,
                        dirty: status.dirty,
                        detail: None,
                    },
                },
            }),
        )
            .into_response(),
    }
}

fn not_ready(database: CheckResult, migrations: MigrationCheck) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(Readiness {
            status: "not_ready",
            checks: ReadinessChecks {
                database,
                migrations,
            },
        }),
    )
        .into_response()
}

#[derive(Debug, Serialize)]
pub struct Version {
    version: &'static str,
    git_sha: &'static str,
    environment: String,
    /// The schema version this binary expects. Read from the embedded
    /// migrations, so this endpoint still answers with the database down.
    migration_version: i64,
}

pub async fn version(State(state): State<AppState>) -> Json<Version> {
    Json(Version {
        version: state.build.version,
        git_sha: state.build.git_sha,
        environment: state.environment.to_string(),
        migration_version: cm_db::migrate::embedded_version(),
    })
}
