//! Saved searches: save the board's current filters, list them, delete them,
//! and the one-click unsubscribe their emails carry.

use crate::extract::{Context, CurrentUser, Json as ValidJson};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
use cm_core::AppError;
use cm_db::repo::saved_searches::{self, SavedSearch};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub name: String,
    /// The board's own query-string vocabulary, flattened: q, trade, zip,
    /// lat/lon/radius_m, timeline, build_type, budget_min. Saving a search is
    /// saving a board URL, so it speaks the board's language.
    #[serde(flatten)]
    pub raw: cm_domain::jobs::RawQuery,
}

/// What a saved search looks like from outside.
#[derive(Debug, Serialize)]
pub struct SavedSearchView {
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_ids: Option<Vec<Uuid>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radius_m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_min_cents: Option<i64>,
    pub notify: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_notified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

fn view(search: SavedSearch) -> SavedSearchView {
    let (lat, lon) = match search.center {
        Some((lat, lon)) => (Some(lat), Some(lon)),
        None => (None, None),
    };
    SavedSearchView {
        id: search.id,
        name: search.name,
        query: search.query,
        trade_ids: search.trade_ids,
        zip: search.postal_code,
        lat,
        lon,
        radius_m: search.radius_m,
        timeline: search.timeline,
        build_type: search.build_type,
        budget_min_cents: search.budget_min_cents,
        notify: search.notify,
        last_notified_at: search.last_notified_at,
        created_at: search.created_at,
    }
}

pub async fn create(
    State(state): State<AppState>,
    Context(_context): Context,
    CurrentUser(caller): CurrentUser,
    ValidJson(body): ValidJson<CreateRequest>,
) -> Result<(StatusCode, Json<SavedSearchView>), AppError> {
    let search = cm_domain::job_alerts::create_saved_search(
        &state.pool,
        state.auth.pepper(),
        caller.user.id,
        &body.name,
        &body.raw,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(view(search))))
}

pub async fn list(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
) -> Result<Json<Vec<SavedSearchView>>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    let searches = saved_searches::list_for_user(&mut conn, caller.user.id).await?;

    Ok(Json(searches.into_iter().map(view).collect()))
}

pub async fn delete(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    if !saved_searches::delete(&mut conn, caller.user.id, id).await? {
        return Err(AppError::NotFound);
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct UnsubscribeParams {
    #[serde(default)]
    pub token: String,
}

/// The link and `List-Unsubscribe-Post` target in every job-alert email.
///
/// Public and sessionless by design (RFC 8058: mail clients post here with no
/// cookies). The HMAC token is the whole authorisation, and it verifies
/// against the id alone — so a repeated click on a long-deleted search still
/// gets its 204 rather than an error nobody can act on.
pub async fn unsubscribe(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<UnsubscribeParams>,
) -> Result<StatusCode, AppError> {
    if !cm_auth::hash::verify_unsubscribe(state.auth.pepper(), &id.to_string(), &params.token) {
        return Err(AppError::invalid("That unsubscribe link is not valid."));
    }

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    saved_searches::set_notify_off(&mut conn, id).await?;

    Ok(StatusCode::NO_CONTENT)
}
