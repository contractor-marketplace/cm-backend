//! Route handlers.

pub mod auth;
pub mod claims;
pub mod contractors;
pub mod jobs;
pub mod me;
pub mod messaging;
pub mod profiles;

use axum::response::Response;
use http::header::SET_COOKIE;

/// Attach `Set-Cookie` headers to a response.
///
/// Appended rather than inserted: a response carries more than one cookie, and
/// `insert` would silently drop all but the last.
pub(crate) fn with_cookies(
    mut response: Response,
    cookies: impl IntoIterator<Item = String>,
) -> Response {
    for cookie in cookies {
        match http::HeaderValue::from_str(&cookie) {
            Ok(value) => {
                response.headers_mut().append(SET_COOKIE, value);
            }
            Err(error) => {
                // Unreachable: every cookie here is built from base64url and
                // ASCII attributes. Logged rather than panicked so a future
                // change cannot take the process down.
                tracing::error!(error = %error, "refused to set a malformed cookie");
            }
        }
    }
    response
}
