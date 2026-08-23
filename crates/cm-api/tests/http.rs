//! HTTP-surface tests.
//!
//! The router is driven directly through `tower::ServiceExt::oneshot`, so no
//! port is bound and the tests are order-independent.

mod common;

use axum::body::Body;
use axum::Router;
use common::{router, unreachable_database_router};
use http::{Request, StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

async fn get(router: Router, path: &str) -> (StatusCode, http::HeaderMap, Value) {
    let response = router
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router should not fail");

    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "body was not JSON ({e}): {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };

    (status, headers, json)
}

#[tokio::test]
async fn healthz_answers_without_touching_the_database() {
    let (status, _, body) = get(unreachable_database_router(), "/healthz").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn readyz_reports_503_when_the_database_is_unreachable() {
    let (status, _, body) = get(unreachable_database_router(), "/readyz").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["checks"]["database"]["status"], "error");

    // The connection error names the host, the port and the role. None of it
    // may reach the caller.
    let rendered = body.to_string();
    for leak in ["127.0.0.1", "nobody", "nothing", "postgres://"] {
        assert!(
            !rendered.contains(leak),
            "readiness leaked {leak}: {rendered}"
        );
    }
}

#[tokio::test]
async fn version_answers_without_touching_the_database() {
    let (status, _, body) = get(unreachable_database_router(), "/version").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["environment"], "development");
    assert_eq!(body["migration_version"], 15);
    assert!(body["git_sha"].is_string());
}

#[sqlx::test(migrations = "../../migrations")]
async fn readyz_is_ready_once_the_schema_matches(pool: PgPool) {
    let (status, _, body) = get(router(pool), "/readyz").await;

    assert_eq!(status, StatusCode::OK, "body was {body}");
    assert_eq!(body["status"], "ready");
    assert_eq!(body["checks"]["database"]["status"], "ok");
    assert_eq!(body["checks"]["migrations"]["applied"], 15);
    assert_eq!(body["checks"]["migrations"]["embedded"], 15);
}

#[sqlx::test(migrations = false)]
async fn readyz_reports_503_when_migrations_have_not_been_applied(pool: PgPool) {
    let (status, _, body) = get(router(pool.clone()), "/readyz").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["checks"]["database"]["status"], "ok");
    assert_eq!(body["checks"]["migrations"]["status"], "error");
    assert!(body["checks"]["migrations"]["detail"]
        .as_str()
        .expect("a detail explaining what to do")
        .contains("no migrations"));

    // ...and becomes ready once they are.
    cm_db::migrate::run(&pool).await.expect("apply migrations");
    let (status, _, _) = get(router(pool), "/readyz").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_unknown_route_returns_the_shared_error_envelope() {
    let (status, headers, body) = get(unreachable_database_router(), "/v1/does-not-exist").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(body["error"]["message"].is_string());
    assert_eq!(
        headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
}

#[tokio::test]
async fn every_response_carries_a_generated_request_id() {
    let (_, headers, _) = get(unreachable_database_router(), "/healthz").await;

    let id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id must be present");
    let parsed = uuid::Uuid::parse_str(id).expect("request id must be a UUID");
    assert_eq!(parsed.get_version_num(), 7, "request ids are UUIDv7");
}

#[tokio::test]
async fn a_client_supplied_request_id_is_replaced_not_trusted() {
    let response = unreachable_database_router()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("x-request-id", "../../etc/passwd injected")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router should not fail");

    let id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id must be present");

    assert_ne!(id, "../../etc/passwd injected");
    assert_eq!(
        uuid::Uuid::parse_str(id)
            .expect("request id must be a UUID")
            .get_version_num(),
        7
    );
}

/// The deployment puts the API behind the same origin as the front end, which
/// is what lets session cookies use the `__Host-` prefix. Permissive CORS
/// headers would quietly undo that decision, so their absence is a test.
#[tokio::test]
async fn no_cross_origin_headers_are_sent() {
    let (_, headers, _) = get(unreachable_database_router(), "/healthz").await;

    for header in [
        "access-control-allow-origin",
        "access-control-allow-credentials",
        "access-control-allow-headers",
        "access-control-allow-methods",
    ] {
        assert!(
            headers.get(header).is_none(),
            "{header} must not be sent: the API is same-origin by design"
        );
    }
}
