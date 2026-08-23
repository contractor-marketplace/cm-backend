//! Router assembly and the middleware stack.

use crate::handlers;
use crate::health;
use crate::middleware::{attach_request_context, require_session};
use crate::request_id::MakeUuidV7RequestId;
use crate::state::AppState;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, patch, post, put};
use axum::Router;
use cm_core::AppError;
use http::{Request, StatusCode};
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::{PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// No request should take this long. A hung upstream must not hold a
/// connection open until the proxy gives up on it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Generous for JSON, small enough that a body cannot be used to exhaust memory
/// on a shared box. Raised per-route when file uploads exist, which is not v1.
const MAX_BODY_BYTES: usize = 256 * 1024;

pub fn build(state: AppState) -> Router {
    // Everything here resolves a session and, for state-changing methods,
    // enforces CSRF. `route_layer` rather than `layer` so an unmatched path
    // 404s without first being asked to authenticate.
    let authenticated = Router::new()
        .route("/v1/me", get(handlers::me::get_me))
        .route("/v1/auth/logout", post(handlers::auth::logout))
        .route("/v1/auth/logout-all", post(handlers::auth::logout_all))
        .route("/v1/auth/password", post(handlers::auth::change_password))
        .route("/v1/auth/link/google", post(handlers::auth::link_google))
        // Homeowner profile.
        .route(
            "/v1/me/homeowner-profile",
            get(handlers::profiles::get).put(handlers::profiles::upsert),
        )
        // Claiming a listing.
        .route("/v1/contractors/{id}/claims", post(handlers::claims::open))
        .route("/v1/me/claims", get(handlers::claims::mine))
        .route(
            "/v1/me/claims/{claim_id}/withdraw",
            post(handlers::claims::withdraw),
        )
        // The claimant's own listing.
        .route(
            "/v1/contractors/{id}",
            patch(handlers::contractors::update_profile),
        )
        // Messaging.
        .route("/v1/conversations", get(handlers::messaging::list))
        .route("/v1/conversations", post(handlers::messaging::start))
        .route(
            "/v1/conversations/{conversation_id}/messages",
            get(handlers::messaging::poll).post(handlers::messaging::send),
        )
        .route(
            "/v1/conversations/{conversation_id}/read",
            post(handlers::messaging::mark_read),
        )
        .route("/v1/blocks", get(handlers::messaging::blocked))
        .route(
            "/v1/blocks/{user_id}",
            put(handlers::messaging::block).delete(handlers::messaging::unblock),
        )
        .route("/v1/reports", post(handlers::messaging::report))
        // Moderation. Role-gated inside the handlers.
        .route("/v1/admin/claims", get(handlers::claims::pending))
        .route(
            "/v1/admin/claims/{claim_id}/decide",
            post(handlers::claims::decide),
        )
        .route("/v1/admin/reports", get(handlers::messaging::open_reports))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));

    // No session to protect yet, so no CSRF token to check. Both are rate
    // limited by address inside the service.
    let public = Router::new()
        .route("/v1/auth/register", post(handlers::auth::register))
        .route("/v1/auth/login", post(handlers::auth::login))
        .route("/v1/auth/google", post(handlers::auth::google_sign_in))
        // The public directory.
        .route("/v1/contractors", get(handlers::contractors::list))
        .route("/v1/contractors/map", get(handlers::contractors::map))
        .route("/v1/contractors/{id}", get(handlers::contractors::detail))
        .route("/v1/trades", get(handlers::contractors::trades))
        .route("/v1/regions", get(handlers::contractors::regions));

    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/version", get(health::version))
        .merge(public)
        .merge(authenticated)
        // Unknown paths get the same error envelope as everything else, rather
        // than axum's empty-bodied default.
        .fallback(not_found)
        .layer(
            ServiceBuilder::new()
                // An inbound x-request-id is discarded before the id is
                // generated. Trusting a client-supplied value would let a
                // caller collide or forge ids in our own logs.
                .map_request(|mut request: Request<axum::body::Body>| {
                    request.headers_mut().remove("x-request-id");
                    request
                })
                .layer(SetRequestIdLayer::x_request_id(MakeUuidV7RequestId))
                // Below SetRequestId so the span can read the header it set.
                .layer(TraceLayer::new_for_http().make_span_with(
                    |request: &Request<axum::body::Body>| {
                        let request_id = request
                            .headers()
                            .get("x-request-id")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("unknown");

                        tracing::info_span!(
                            "http",
                            request_id = %request_id,
                            method = %request.method(),
                            // The path only. A query string can carry
                            // caller-supplied values we do not want in logs.
                            path = %request.uri().path(),
                        )
                    },
                ))
                .layer(PropagateRequestIdLayer::x_request_id())
                // A panic must not take the worker down or answer with an empty
                // body; it becomes an ordinary internal error.
                .layer(CatchPanicLayer::custom(handle_panic))
                // 504 rather than the default 408: the request was fine, this
                // server failed to answer it in time.
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::GATEWAY_TIMEOUT,
                    REQUEST_TIMEOUT,
                ))
                // axum's own limit rather than tower-http's: it leaves the response
                // body type untouched, which the timeout layer above requires.
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
                // Innermost, so it runs after the request id exists and can
                // carry it. Applied to every route, public and protected, so
                // no handler has to assemble the context itself.
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    attach_request_context,
                )),
        )
        .with_state(state)
}

async fn not_found() -> AppError {
    AppError::NotFound
}

fn handle_panic(
    panic: Box<dyn std::any::Any + Send + 'static>,
) -> http::Response<axum::body::Body> {
    let detail = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&'static str>().copied())
        .unwrap_or("unknown panic");

    // Logged with the detail, answered without it.
    axum::response::IntoResponse::into_response(AppError::internal(format!(
        "handler panicked: {detail}"
    )))
}
