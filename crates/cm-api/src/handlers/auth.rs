//! Registration, login, logout and password change.

use crate::extract::{Context, CurrentUser, Json as ValidJson};
use crate::handlers::{me::user_view, with_cookies};
use crate::state::AppState;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use cm_auth::cookie;
use cm_auth::{IssuedSession, LoginOutcome};
use cm_core::AppError;
use cm_db::repo::oauth::Provider;
use cm_db::repo::users::AccountType;
use http::StatusCode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub display_name: String,
    pub password: String,
    /// "homeowner" or "contractor". Required, and fixed for the life of the
    /// account: the two sides of the marketplace are mutually exclusive, so
    /// there is no later step at which this could be chosen instead.
    pub account_type: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct FederatedSignInRequest {
    /// The Firebase ID token the browser obtained. Verified, used once, and
    /// never stored.
    ///
    /// Note what this body does *not* carry: which provider it came from. That
    /// is fixed by the route, so a token can never nominate the identity slot
    /// it wants to be checked against.
    pub id_token: String,

    /// Which side of the marketplace to create, if this token turns out to
    /// belong to nobody yet. Optional because signing in does not need it, and
    /// ignored outright when the identity already resolves to an account — an
    /// account never changes sides, so this can only ever describe a new one.
    ///
    /// The sign-up page sends it; the sign-in page does not.
    #[serde(default)]
    pub account_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    user: crate::handlers::me::UserView,
    csrf_token: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

fn session_response(status: StatusCode, outcome: &LoginOutcome, state: &AppState) -> Response {
    let body = Json(SessionResponse {
        user: user_view(&outcome.user),
        csrf_token: outcome.session.csrf_token.clone(),
        expires_at: outcome.session.absolute_expires_at,
    });

    with_cookies(
        (status, body).into_response(),
        state.auth.session_cookies(&outcome.session),
    )
}

pub async fn register(
    State(state): State<AppState>,
    Context(context): Context,
    ValidJson(body): ValidJson<RegisterRequest>,
) -> Result<Response, AppError> {
    let account_type = cm_db::repo::users::AccountType::parse_request(&body.account_type)?;

    let outcome = state
        .auth
        .register(
            &state.pool,
            &body.email,
            &body.display_name,
            &body.password,
            account_type,
            &context,
        )
        .await?;

    Ok(session_response(StatusCode::CREATED, &outcome, &state))
}

pub async fn login(
    State(state): State<AppState>,
    Context(context): Context,
    ValidJson(body): ValidJson<LoginRequest>,
) -> Result<Response, AppError> {
    let outcome = state
        .auth
        .login(&state.pool, &body.email, &body.password, &context)
        .await?;

    Ok(session_response(StatusCode::OK, &outcome, &state))
}

pub async fn google_sign_in(
    State(state): State<AppState>,
    Context(context): Context,
    ValidJson(body): ValidJson<FederatedSignInRequest>,
) -> Result<Response, AppError> {
    // Parsed here so an unknown value is a 400 naming the options, rather than
    // reaching the service as something it has to guess about.
    let account_type = body
        .account_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(AccountType::parse_request)
        .transpose()?;

    let outcome = state
        .auth
        .sign_in_with_provider(
            &state.pool,
            Provider::Google,
            &body.id_token,
            account_type,
            &context,
        )
        .await?;

    Ok(session_response(StatusCode::OK, &outcome, &state))
}

pub async fn facebook_sign_in(
    State(state): State<AppState>,
    Context(context): Context,
    ValidJson(body): ValidJson<FederatedSignInRequest>,
) -> Result<Response, AppError> {
    // Parsed here so an unknown value is a 400 naming the options, rather than
    // reaching the service as something it has to guess about.
    let account_type = body
        .account_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(AccountType::parse_request)
        .transpose()?;

    let outcome = state
        .auth
        .sign_in_with_provider(
            &state.pool,
            Provider::Facebook,
            &body.id_token,
            account_type,
            &context,
        )
        .await?;

    Ok(session_response(StatusCode::OK, &outcome, &state))
}

pub async fn link_google(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    ValidJson(body): ValidJson<FederatedSignInRequest>,
) -> Result<StatusCode, AppError> {
    state
        .auth
        .link_provider(
            &state.pool,
            caller.user.id,
            Provider::Google,
            &body.id_token,
            &context,
        )
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn link_facebook(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    ValidJson(body): ValidJson<FederatedSignInRequest>,
) -> Result<StatusCode, AppError> {
    state
        .auth
        .link_provider(
            &state.pool,
            caller.user.id,
            Provider::Facebook,
            &body.id_token,
            &context,
        )
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn logout(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
) -> Result<Response, AppError> {
    state
        .auth
        .logout(&state.pool, caller.session_id, caller.user.id, &context)
        .await?;

    Ok(cleared_response())
}

pub async fn logout_all(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
) -> Result<Response, AppError> {
    state
        .auth
        .logout_all(&state.pool, caller.user.id, &context)
        .await?;

    Ok(cleared_response())
}

/// 204 with both cookies expired, so a client that ignores the body still ends
/// up holding nothing.
fn cleared_response() -> Response {
    with_cookies(
        StatusCode::NO_CONTENT.into_response(),
        [cookie::clear_session(), cookie::clear_csrf()],
    )
}

pub async fn change_password(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    ValidJson(body): ValidJson<ChangePasswordRequest>,
) -> Result<Response, AppError> {
    let session: IssuedSession = state
        .auth
        .change_password(
            &state.pool,
            caller.user.id,
            caller.session_id,
            &body.current_password,
            &body.new_password,
            &context,
        )
        .await?;

    let body = Json(SessionResponse {
        user: user_view(&caller.user),
        csrf_token: session.csrf_token.clone(),
        expires_at: session.absolute_expires_at,
    });

    Ok(with_cookies(
        (StatusCode::OK, body).into_response(),
        state.auth.session_cookies(&session),
    ))
}
