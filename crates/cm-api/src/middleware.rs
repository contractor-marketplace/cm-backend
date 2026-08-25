//! Session and CSRF middleware.

use crate::state::AppState;
use axum::extract::{ConnectInfo, State};
use axum::middleware::Next;
use axum::response::Response;
use cm_auth::cookie;
use cm_auth::csrf;
use cm_auth::RequestContext;
use cm_core::AppError;
use http::Request;
use std::net::SocketAddr;

/// Resolve the session, and enforce CSRF on anything that changes state.
///
/// The two are done together on purpose. If authentication and CSRF were
/// separate layers, a route could be added behind one and not the other, and
/// nothing would notice; here, a route that can see a caller has necessarily
/// passed both.
pub async fn require_session(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, AppError> {
    let token = request
        .headers()
        .get(http::header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie::read(header, cookie::SESSION_COOKIE))
        .ok_or(AppError::Unauthenticated)?;

    let authenticated = state.auth.authenticate(&state.pool, token).await?;

    if csrf::method_requires_check(request.method()) {
        let presented = request
            .headers()
            .get(csrf::CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let origin = request
            .headers()
            .get(http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        state.auth.verify_csrf(
            authenticated.session_id,
            presented.as_deref(),
            origin.as_deref(),
        )?;
    }

    request.extensions_mut().insert(authenticated);
    Ok(next.run(request).await)
}

/// A session if one is presented, and no opinion if not.
///
/// This exists for one shape of route: public, readable by anyone, but showing
/// more to a caller we can identify. The jobs board is the case — a signed-out
/// visitor sees a redacted listing, and a contractor sees the detail.
///
/// It deliberately does NOT check CSRF, which would be a hole on any route that
/// changes state. It is safe here only because it is attached exclusively to
/// read-only routes, and that "exclusively" is enforced rather than trusted:
/// `every_public_route_is_read_only` in the router tests fails the build if a
/// mutating method is ever registered on the public router.
///
/// It never rejects. A stale or forged cookie resolves to `None` and the caller
/// simply gets the anonymous view, because a public route answering 401 would
/// be a worse bug than showing someone slightly less than they were owed.
pub async fn attach_optional_session(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let resolved = match request
        .headers()
        .get(http::header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie::read(header, cookie::SESSION_COOKIE))
    {
        Some(token) => state.auth.authenticate(&state.pool, token).await.ok(),
        None => None,
    };

    request.extensions_mut().insert(OptionalSession(resolved));
    next.run(request).await
}

/// Wrapper so the extension type is distinct from `Authenticated`: a route that
/// wants a guaranteed caller must not accidentally satisfy itself with a
/// maybe-caller.
#[derive(Debug, Clone)]
pub struct OptionalSession(pub Option<cm_auth::Authenticated>);

/// Build the transport context once, at the edge, and put it in the request.
///
/// Handlers read it with the `Context` extractor rather than assembling it
/// themselves. Each handler doing its own assembly is how the client address
/// silently went missing from logout and password-change audit events: a
/// handler that receives `HeaderMap` but not the request has no `ConnectInfo`
/// to find, and nothing complains. Built in one place, it cannot be dropped in
/// one place.
pub async fn attach_request_context(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let context = request_context(
        request.headers(),
        request.extensions(),
        state.trust_proxy_headers,
    );
    request.extensions_mut().insert(context);
    next.run(request).await
}

/// Everything the auth service needs to know about the transport.
pub fn request_context(
    headers: &http::HeaderMap,
    extensions: &http::Extensions,
    trust_proxy_headers: bool,
) -> RequestContext {
    let peer = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| *address);

    RequestContext {
        client_ip: crate::client_ip::resolve(headers, peer, trust_proxy_headers),
        user_agent: headers
            .get(http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            // The column is bounded; truncate rather than reject a request over
            // a header nobody controls.
            .map(|value| value.chars().take(512).collect()),
        request_id: headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    }
}
