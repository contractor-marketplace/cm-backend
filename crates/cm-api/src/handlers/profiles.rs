//! Homeowner profiles.

use crate::extract::{CurrentUser, Json as ValidJson};
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use cm_core::AppError;
use cm_db::repo::profiles::{self, HomeownerProfile};
use cm_db::repo::reference;
use serde::Deserialize;

pub async fn get(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
) -> Result<Json<Option<HomeownerProfile>>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    // An absent profile is `null`, not a 404: mid-onboarding is a valid state,
    // and a 404 would make the client treat it as an error.
    Ok(Json(profiles::find(&mut conn, caller.user.id).await?))
}

#[derive(Debug, Deserialize)]
pub struct UpsertRequest {
    pub display_name: String,
    pub postal_code: Option<String>,
    pub contact_phone: Option<String>,
}

pub async fn upsert(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    ValidJson(body): ValidJson<UpsertRequest>,
) -> Result<Json<HomeownerProfile>, AppError> {
    let display_name = body.display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 120 {
        return Err(AppError::invalid(
            "A display name is required, and must be under 120 characters.",
        ));
    }

    let postal_code = match body.postal_code.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(code) if code.len() == 5 && code.chars().all(|c| c.is_ascii_digit()) => {
            Some(code.to_owned())
        }
        Some(_) => return Err(AppError::invalid("Enter a valid five-digit ZIP code.")),
    };

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    let region_id = match &postal_code {
        Some(code) => reference::find_zcta(&mut conn, code).await?.map(|r| r.id),
        None => None,
    };

    let profile = profiles::upsert(
        &mut conn,
        caller.user.id,
        display_name,
        postal_code.as_deref(),
        body.contact_phone.as_deref(),
        region_id,
    )
    .await?;

    Ok(Json(profile))
}
