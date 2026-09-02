//! Registration, login, logout and password change.

use crate::extract::{Context, CurrentUser, Json as ValidJson};
use crate::handlers::{me::user_view, with_cookies};
use crate::state::AppState;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use cm_auth::cookie;
use cm_auth::login_code::{device_cookie, DEVICE_COOKIE};
use cm_auth::{Challenge, IssuedSession, LoginOutcome, LoginResult};
use cm_core::AppError;
use cm_db::repo::oauth::Provider;
use cm_db::repo::users::AccountType;
use http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
pub struct VerifyCodeRequest {
    pub challenge_id: Uuid,
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct ResendCodeRequest {
    pub challenge_id: Uuid,
}

/// A sign-in waiting on its emailed code. 202: the request was accepted, the
/// session it asked for does not exist yet.
#[derive(Debug, Serialize)]
pub struct ChallengeResponse {
    challenge_id: Uuid,
    email: String,
}

fn challenge_response(challenge: Challenge) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(ChallengeResponse {
            challenge_id: challenge.challenge_id,
            email: challenge.email,
        }),
    )
        .into_response()
}

/// The remembered-device cookie, if the browser sent one.
fn device_from(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(http::header::COOKIE)?.to_str().ok()?;
    cookie::read(header, DEVICE_COOKIE).map(str::to_owned)
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

    /// The address the provider showed the browser during the popup.
    ///
    /// A fallback, not an authority: the Firebase console mode this product
    /// requires (account linking off) strips the email claim from OAuth
    /// tokens, so the verified token often cannot say what address the person
    /// signed in with — while the popup result still can. The service prefers
    /// whatever the token itself carries, consults this only when creating an
    /// account, and stores it **unverified**: it is exactly as trustworthy as
    /// an address typed into the email form, and earns verified status the
    /// same way, by the emailed code.
    #[serde(default)]
    pub email: Option<String>,
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

    let challenge = state
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

    Ok(challenge_response(challenge))
}

pub async fn login(
    State(state): State<AppState>,
    Context(context): Context,
    headers: HeaderMap,
    ValidJson(body): ValidJson<LoginRequest>,
) -> Result<Response, AppError> {
    let device = device_from(&headers);

    let result = state
        .auth
        .login(
            &state.pool,
            &body.email,
            &body.password,
            device.as_deref(),
            &context,
        )
        .await?;

    Ok(match result {
        LoginResult::Session(outcome) => session_response(StatusCode::OK, &outcome, &state),
        LoginResult::Challenged(challenge) => challenge_response(challenge),
    })
}

/// Exchange a challenge and its emailed code for a session. The response also
/// marks this browser as remembered, so the next login is one step.
pub async fn verify_login_code(
    State(state): State<AppState>,
    Context(context): Context,
    ValidJson(body): ValidJson<VerifyCodeRequest>,
) -> Result<Response, AppError> {
    let (outcome, device) = state
        .auth
        .verify_login_code(&state.pool, body.challenge_id, &body.code, &context)
        .await?;

    Ok(with_cookies(
        session_response(StatusCode::OK, &outcome, &state),
        [device_cookie(&device)],
    ))
}

/// Re-send a challenge's code. The reply carries a fresh challenge id — the
/// old code and id stop working the moment this succeeds.
pub async fn resend_login_code(
    State(state): State<AppState>,
    Context(context): Context,
    ValidJson(body): ValidJson<ResendCodeRequest>,
) -> Result<Response, AppError> {
    let challenge = state
        .auth
        .resend_login_code(&state.pool, body.challenge_id, &context)
        .await?;

    Ok(challenge_response(challenge))
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
            body.email.as_deref(),
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
            body.email.as_deref(),
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

#[derive(Debug, Deserialize)]
pub struct PasswordResetRequest {
    pub email: String,
}

#[derive(Debug, Deserialize)]
pub struct PasswordResetConfirm {
    pub token: String,
    pub new_password: String,
}

/// 204 whether or not the address has an account: the response must not say.
pub async fn request_password_reset(
    State(state): State<AppState>,
    Context(context): Context,
    ValidJson(body): ValidJson<PasswordResetRequest>,
) -> Result<StatusCode, AppError> {
    state
        .auth
        .request_password_reset(&state.pool, &body.email, &context)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn confirm_password_reset(
    State(state): State<AppState>,
    Context(context): Context,
    ValidJson(body): ValidJson<PasswordResetConfirm>,
) -> Result<StatusCode, AppError> {
    state
        .auth
        .confirm_password_reset(&state.pool, &body.token, &body.new_password, &context)
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
