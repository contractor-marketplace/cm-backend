//! Request extractors.

use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use cm_auth::{Authenticated, RequestContext};
use cm_core::AppError;
use http::request::Parts;

/// The authenticated caller.
///
/// Reads what the session middleware put in the request extensions rather than
/// authenticating itself, so a route can only see a caller if it is behind that
/// middleware — and therefore only if the CSRF check has also run.
#[derive(Debug, Clone)]
pub struct CurrentUser(pub Authenticated);

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Authenticated>()
            .cloned()
            .map(Self)
            .ok_or(AppError::Unauthenticated)
    }
}

/// What the transport knows about this request: client address, user agent,
/// request id.
///
/// Populated by the `attach_request_context` layer, which sees the whole
/// request. A handler cannot construct one by accident from the pieces it
/// happens to have been given.
#[derive(Debug, Clone)]
pub struct Context(pub RequestContext);

impl<S: Send + Sync> FromRequestParts<S> for Context {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<RequestContext>()
            .cloned()
            .map(Self)
            // Unreachable while the layer is installed; an error rather than a
            // default, so a missing layer fails loudly instead of quietly
            // logging every request as coming from nowhere.
            .ok_or_else(|| AppError::internal("the request-context layer is not installed"))
    }
}

/// JSON body extraction that fails in our own error envelope.
///
/// axum's own rejection renders a bare string with a different shape, so a
/// malformed body would be the one response in the API that does not look like
/// the others.
#[derive(Debug, Clone, Copy)]
pub struct Json<T>(pub T);

impl<T> FromRequest<AppState> for Json<T>
where
    T: serde::de::DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            // The rejection text names the offending field and type, which is
            // useful to a client and reveals nothing about the server.
            Err(rejection) => Err(match rejection {
                JsonRejection::JsonDataError(error) => AppError::invalid(error.body_text()),
                JsonRejection::JsonSyntaxError(_) => {
                    AppError::invalid("The request body is not valid JSON.")
                }
                JsonRejection::MissingJsonContentType(_) => {
                    AppError::invalid("Expected a Content-Type of application/json.")
                }
                other => AppError::invalid(other.body_text()),
            }),
        }
    }
}
