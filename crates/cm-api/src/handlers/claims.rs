//! Claiming a listing, and deciding those claims.

use crate::extract::{Context, CurrentUser, Json as ValidJson};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use cm_core::AppError;
use cm_db::repo::claims::{Claim, ClaimMethod};
use cm_db::repo::users::Role;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct OpenClaimRequest {
    /// How the claimant proposes to prove the listing is theirs.
    pub method: String,
    /// Their own assertion. Stored, never trusted on its own: what decides a
    /// claim is a `verification_checks` row written by whoever checked.
    #[serde(default)]
    pub evidence: serde_json::Value,
}

pub async fn open(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    Path(contractor_id): Path<Uuid>,
    ValidJson(body): ValidJson<OpenClaimRequest>,
) -> Result<(StatusCode, Json<Claim>), AppError> {
    // The two sides of the marketplace are mutually exclusive, and claiming a
    // listing is the contractor's. A homeowner account is refused here rather
    // than at the database trigger, so the caller gets an explanation instead
    // of a 500.
    if !caller.user.account_type.may_claim_a_listing() {
        // Only a contractor account can claim a listing.
        return Err(AppError::Forbidden);
    }

    let method = ClaimMethod::parse(&body.method).ok_or_else(|| {
        AppError::invalid(format!(
            "unknown method \"{}\"; expected one of {}",
            body.method,
            ClaimMethod::ALL
                .iter()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;

    let claim = cm_domain::claims::open(
        &state.pool,
        contractor_id,
        caller.user.id,
        method,
        body.evidence,
        context.request_id,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(claim)))
}

pub async fn mine(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
) -> Result<Json<Vec<Claim>>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    Ok(Json(
        cm_db::repo::claims::for_user(&mut conn, caller.user.id).await?,
    ))
}

pub async fn withdraw(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    Path(claim_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    cm_domain::claims::withdraw(&state.pool, claim_id, caller.user.id, context.request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── administrative ──────────────────────────────────────────────────────────

/// Refuse anyone without a moderating role.
///
/// 403 rather than 404 here: an admin surface is not a secret, and a signed-in
/// user being told "not for you" is clearer than a phantom missing page.
fn require_moderator(caller: &cm_auth::Authenticated) -> Result<(), AppError> {
    if caller.has_role(Role::Admin) || caller.has_role(Role::Moderator) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

pub async fn pending(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
) -> Result<Json<Vec<Claim>>, AppError> {
    require_moderator(&caller)?;

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    Ok(Json(cm_db::repo::claims::pending(&mut conn, 100).await?))
}

#[derive(Debug, Deserialize)]
pub struct DecideRequest {
    pub approve: bool,
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DecisionResponse {
    claim: Claim,
    verified: bool,
    verification_reason: String,
}

pub async fn decide(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    Path(claim_id): Path<Uuid>,
    ValidJson(body): ValidJson<DecideRequest>,
) -> Result<Json<DecisionResponse>, AppError> {
    require_moderator(&caller)?;

    let decision = cm_domain::claims::decide(
        &state.pool,
        claim_id,
        body.approve,
        caller.user.id,
        body.note,
        context.request_id,
    )
    .await?;

    Ok(Json(DecisionResponse {
        claim: decision.claim,
        verified: decision.verified,
        verification_reason: decision.verification_reason,
    }))
}
