//! The application error taxonomy and its HTTP rendering.
//!
//! One rule governs the whole file: an internal error tells the client nothing.
//! The cause is logged with the request id so an operator can find it; the
//! response carries a stable code and a generic message. Leaking a database
//! error string to a caller is how schema details escape.

use axum_core::response::{IntoResponse, Response};
use http::{header, StatusCode};
use std::time::Duration;

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Every failure the API can produce, independent of transport.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The addressed resource does not exist, or the caller may not know that
    /// it does. Authorization failures deliberately land here for resources
    /// whose existence is itself private.
    #[error("resource not found")]
    NotFound,

    /// The request was understood and rejected. The message is written for the
    /// caller and is safe to return verbatim.
    #[error("{message}")]
    Invalid { message: String },

    /// No valid session. Deliberately carries no detail: "no cookie", "expired"
    /// and "revoked" are the same answer, because distinguishing them tells an
    /// attacker which of their guesses was closest.
    #[error("authentication required")]
    Unauthenticated,

    /// Authenticated, but not permitted. Used only where the caller is already
    /// allowed to know the resource exists; everything else answers `NotFound`.
    #[error("forbidden")]
    Forbidden,

    /// The request conflicts with existing state.
    #[error("{message}")]
    Conflict { message: String },

    /// Rate limited. The wait is returned so a client can back off correctly
    /// rather than retrying in a tight loop.
    #[error("too many requests")]
    TooManyRequests { retry_after: Duration },

    /// A dependency this request needed is not available right now.
    #[error("service unavailable: {message}")]
    Unavailable { message: String },

    /// Anything unexpected. The source is logged, never returned.
    #[error("internal error")]
    Internal(#[source] BoxError),
}

impl AppError {
    pub fn internal(source: impl Into<BoxError>) -> Self {
        Self::Internal(source.into())
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict {
            message: message.into(),
        }
    }

    /// A stable machine-readable code. Clients branch on this, not on prose.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Invalid { .. } => "invalid_request",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::Conflict { .. } => "conflict",
            Self::TooManyRequests { .. } => "too_many_requests",
            Self::Unavailable { .. } => "unavailable",
            Self::Internal(_) => "internal_error",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Invalid { .. } => StatusCode::BAD_REQUEST,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Conflict { .. } => StatusCode::CONFLICT,
            Self::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// What the client is told. For internal errors this is deliberately empty
    /// of detail.
    pub fn public_message(&self) -> String {
        match self {
            Self::NotFound => "The requested resource was not found.".to_owned(),
            Self::Invalid { message } => message.clone(),
            Self::Unauthenticated => "Authentication is required.".to_owned(),
            Self::Forbidden => "You do not have permission to do that.".to_owned(),
            Self::Conflict { message } => message.clone(),
            Self::TooManyRequests { .. } => "Too many requests. Try again shortly.".to_owned(),
            Self::Unavailable { message } => message.clone(),
            Self::Internal(_) => {
                "The server encountered an internal error. The failure has been logged.".to_owned()
            }
        }
    }

    fn body(&self) -> String {
        // Hand-rolled rather than serde_json::to_string on a struct, because
        // this must never itself fail: an error path that can error is a
        // 500-inside-a-500.
        format!(
            "{{\"error\":{{\"code\":{},\"message\":{}}}}}",
            json_string(self.code()),
            json_string(&self.public_message())
        )
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Logged here, at the single point every error passes through, so no
        // handler has to remember to do it.
        match &self {
            Self::Internal(source) => {
                tracing::error!(error.code = self.code(), error.cause = %source, "request failed");
            }
            Self::Unavailable { message } => {
                tracing::warn!(error.code = self.code(), error.message = %message, "request failed");
            }
            other => {
                tracing::debug!(error.code = other.code(), "request rejected");
            }
        }

        let mut response = (
            self.status(),
            [(header::CONTENT_TYPE, "application/json")],
            self.body(),
        )
            .into_response();

        // Deliberately no `WWW-Authenticate` on 401: this API authenticates
        // with a cookie, and a challenge header makes browsers raise a
        // basic-auth dialog the product has no use for.
        if let Self::TooManyRequests { retry_after } = &self {
            let seconds = retry_after.as_secs().max(1);
            if let Ok(value) = http::HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }

        response
    }
}

/// Minimal JSON string escaping for the error envelope.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_statuses_and_codes_are_stable() {
        assert_eq!(AppError::Unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(AppError::Unauthenticated.code(), "unauthenticated");
        assert_eq!(AppError::Forbidden.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            AppError::conflict("that address is already registered").status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            AppError::TooManyRequests {
                retry_after: std::time::Duration::from_secs(30)
            }
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn a_rate_limited_response_tells_the_client_how_long_to_wait() {
        let response = AppError::TooManyRequests {
            retry_after: std::time::Duration::from_secs(42),
        }
        .into_response();

        assert_eq!(
            response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("42")
        );
    }

    /// A sub-second wait must not round down to `Retry-After: 0`, which a
    /// client would read as "retry immediately".
    #[test]
    fn a_sub_second_wait_still_asks_for_at_least_one_second() {
        let response = AppError::TooManyRequests {
            retry_after: std::time::Duration::from_millis(120),
        }
        .into_response();

        assert_eq!(
            response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    /// Cookie-authenticated APIs must not send a challenge: browsers answer it
    /// with a basic-auth dialog.
    #[test]
    fn a_401_does_not_send_an_authentication_challenge() {
        let response = AppError::Unauthenticated.into_response();
        assert!(response
            .headers()
            .get(http::header::WWW_AUTHENTICATE)
            .is_none());
    }

    #[test]
    fn statuses_and_codes_are_stable() {
        assert_eq!(AppError::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(AppError::NotFound.code(), "not_found");

        let invalid = AppError::invalid("radius_m must be a positive integer");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(invalid.code(), "invalid_request");

        let unavailable = AppError::unavailable("database is unreachable");
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);

        let internal = AppError::internal(std::io::Error::other("boom"));
        assert_eq!(internal.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn an_internal_error_never_leaks_its_cause() {
        let secret = "connection to 10.0.0.5 failed: password authentication failed for user";
        let error = AppError::internal(std::io::Error::other(secret));

        let public = error.public_message();
        assert!(!public.contains("password"), "leaked: {public}");
        assert!(!public.contains("10.0.0.5"), "leaked: {public}");

        let body = error.body();
        assert!(!body.contains("password"), "leaked: {body}");
        assert!(body.contains("internal_error"));

        // The cause is still reachable for the log line.
        assert!(error.source().is_some());
    }

    #[test]
    fn a_client_facing_message_is_returned_verbatim() {
        let error = AppError::invalid("zip must be five digits");
        assert!(error.body().contains("zip must be five digits"));
    }

    #[test]
    fn the_error_envelope_is_valid_json_even_with_hostile_input() {
        let error = AppError::invalid("quote \" backslash \\ newline \n tab \t control \u{1}");
        let parsed: serde_json::Value =
            serde_json::from_str(&error.body()).expect("body must be valid JSON");

        assert_eq!(parsed["error"]["code"], "invalid_request");
        assert!(parsed["error"]["message"]
            .as_str()
            .expect("message is a string")
            .contains("backslash"));
    }

    // `source()` is used above; keep the trait in scope for the test module.
    use std::error::Error as _;
}
