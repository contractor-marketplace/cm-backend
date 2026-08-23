//! The public contractor directory, and the claimant's own edit surface.

use crate::extract::{CurrentUser, Json as ValidJson};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
use cm_core::AppError;
use cm_db::repo::contractors::{self, AddressVisibility, ProfileUpdate, PublicContractor};
use cm_db::repo::{claims, reference, search};
use cm_domain::search as search_input;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct ListResponse {
    contractors: Vec<PublicContractor>,
    /// Absent when this is the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
    /// Filters that could not be parsed and were dropped, so "why did my filter
    /// do nothing" is answerable without reading the source.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ignored_filters: Vec<String>,
}

async fn trade_ids(state: &AppState, trade: Option<&str>) -> Result<Vec<Uuid>, AppError> {
    let Some(trade) = trade else {
        return Ok(Vec::new());
    };
    let slugs: Vec<String> = trade
        .split(',')
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .map(str::to_owned)
        .collect();

    if slugs.is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    reference::trade_ids_for_slugs(&mut conn, &slugs).await
}

pub async fn list(
    State(state): State<AppState>,
    Query(raw): Query<search_input::RawQuery>,
) -> Result<Json<ListResponse>, AppError> {
    let ids = trade_ids(&state, raw.trade.as_deref()).await?;
    let request = search_input::parse(&raw, ids)?;

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    let page = search::list(
        &mut conn,
        &request.filters,
        request.sort,
        request.limit,
        request.cursor.as_ref(),
    )
    .await?;

    Ok(Json(ListResponse {
        contractors: page.contractors,
        next_cursor: page.next_cursor.as_ref().map(search_input::encode_cursor),
        ignored_filters: request.ignored,
    }))
}

#[derive(Debug, Serialize)]
pub struct MapResponse {
    points: Vec<MapPoint>,
    /// True when the viewport holds more than the cap. A map that silently
    /// omits pins is worse than one that says it is showing a subset.
    truncated: bool,
    limit: i64,
}

#[derive(Debug, Serialize)]
pub struct MapPoint {
    id: Uuid,
    display_name: String,
    verified: bool,
    lat: f64,
    lon: f64,
    location_precision: contractors::PublicPointSource,
}

pub async fn map(
    State(state): State<AppState>,
    Query(raw): Query<search_input::RawQuery>,
) -> Result<Json<MapResponse>, AppError> {
    let ids = trade_ids(&state, raw.trade.as_deref()).await?;
    let request = search_input::parse(&raw, ids)?;

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    let (found, truncated) =
        search::map_points(&mut conn, &request.filters, search::MAX_MAP_POINTS).await?;

    let points = found
        .into_iter()
        .filter_map(|c| {
            // A contractor with no published point has no pin. It is not given
            // a guessed one.
            Some(MapPoint {
                id: c.id,
                display_name: c.display_name,
                verified: c.verified,
                lat: c.lat?,
                lon: c.lon?,
                location_precision: c.location_precision,
            })
        })
        .collect();

    Ok(Json(MapResponse {
        points,
        truncated,
        limit: search::MAX_MAP_POINTS,
    }))
}

#[derive(Debug, Serialize)]
pub struct DetailResponse {
    #[serde(flatten)]
    contractor: PublicContractor,
    /// Why the badge is, or is not, present. Stored when it is computed, and
    /// written for a person to read: "CSLB licence 1047382 is suspended as of
    /// the last import" is an answer; a bare `false` is not.
    verification_reason: Option<String>,
    /// When the licence register this is derived from was last refreshed, so a
    /// client can say "as of" rather than implying it is live.
    license_data_as_of: Option<chrono::NaiveDate>,
    /// The evidence behind the badge.
    verification: Vec<VerificationView>,
}

#[derive(Debug, Serialize)]
pub struct VerificationView {
    kind: String,
    outcome: String,
    observed_at: chrono::DateTime<chrono::Utc>,
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DetailResponse>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;

    // Accepts an id or a slug, so a shareable URL does not have to be a UUID.
    let contractor = match Uuid::parse_str(&id) {
        Ok(uuid) => search::find_public(&mut conn, uuid).await?,
        Err(_) => search::find_public_by_slug(&mut conn, &id).await?,
    }
    .ok_or(AppError::NotFound)?;

    let verification_reason: Option<String> =
        sqlx::query_scalar("SELECT verification_reason FROM contractors WHERE id = $1")
            .bind(contractor.id)
            .fetch_one(&mut *conn)
            .await
            .map_err(AppError::internal)?;

    let license_data_as_of = cm_db::repo::licenses::latest_successful_snapshot(&mut conn)
        .await?
        .and_then(|(_, snapshot_date, _)| snapshot_date);

    let verification = claims::checks_for_contractor(&mut conn, contractor.id, 20)
        .await?
        .into_iter()
        .map(|(kind, outcome, _evidence, observed_at)| VerificationView {
            kind,
            outcome,
            observed_at,
        })
        .collect();

    Ok(Json(DetailResponse {
        contractor,
        verification_reason,
        license_data_as_of,
        verification,
    }))
}

pub async fn trades(
    State(state): State<AppState>,
) -> Result<Json<Vec<reference::Trade>>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    Ok(Json(reference::all_trades(&mut conn).await?))
}

pub async fn regions(
    State(state): State<AppState>,
) -> Result<Json<Vec<reference::Region>>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    Ok(Json(reference::list_zctas(&mut conn).await?))
}

#[derive(Debug, Deserialize)]
pub struct UpdateProfileRequest {
    pub bio: Option<String>,
    pub website_url: Option<String>,
    pub public_phone: Option<String>,
    pub accepts_dm: Option<bool>,
    pub address_visibility: Option<String>,
    /// Present only so a client that sends it gets a clear refusal instead of
    /// silently having it ignored — which would teach the client it worked.
    #[serde(default)]
    pub verified: Option<serde_json::Value>,
}

pub async fn update_profile(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(contractor_id): Path<Uuid>,
    ValidJson(body): ValidJson<UpdateProfileRequest>,
) -> Result<Json<PublicContractor>, AppError> {
    if body.verified.is_some() {
        return Err(AppError::invalid(
            "\"verified\" is computed from licence and claim state and cannot be set.",
        ));
    }

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;

    // Only the approved claimant may edit, and a non-claimant is told the
    // listing is not theirs rather than that it exists and is someone else's.
    let owner = contractors::claimed_by(&mut conn, caller.user.id).await?;
    if owner != Some(contractor_id) {
        return Err(AppError::Forbidden);
    }

    let visibility = match body.address_visibility.as_deref() {
        None => None,
        Some("protected") => Some(AddressVisibility::Protected),
        Some("public") => Some(AddressVisibility::Public),
        Some(other) => {
            return Err(AppError::invalid(format!(
                "unknown address_visibility \"{other}\"; expected protected or public"
            )))
        }
    };

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    contractors::update_profile(
        &mut tx,
        contractor_id,
        &ProfileUpdate {
            bio: body.bio,
            website_url: body.website_url,
            public_phone: body.public_phone,
            accepts_dm: body.accepts_dm,
            address_visibility: visibility,
        },
    )
    .await?;

    // Turning publication off has to take effect now, not at the next geocode.
    if visibility.is_some() {
        cm_domain::location::reapply(&mut tx, contractor_id).await?;
    }
    tx.commit().await.map_err(AppError::internal)?;

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    search::find_public(&mut conn, contractor_id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}
