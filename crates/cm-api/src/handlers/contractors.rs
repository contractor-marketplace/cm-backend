//! The public contractor directory, and the claimant's own edit surface.

use crate::extract::{CurrentUser, Json as ValidJson};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
use cm_core::AppError;
use cm_db::repo::contractors::{self, AddressVisibility, ProfileUpdate, PublicContractor};
use cm_db::repo::{claims, reference, reviews, search};
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

/// The trades a free-text query is asking for.
///
/// "hvac" is not a business name and never will be, so matching it against
/// names and bios finds nothing however well that matching works. Resolving it
/// through the alias vocabulary first turns a hopeless text query into an
/// indexed semi-join. Applied to the list and the map alike, or the two would
/// disagree about what matches — which is the one thing the shared predicate
/// exists to prevent.
async fn query_trades(
    conn: &mut sqlx::PgConnection,
    query: Option<&str>,
) -> Result<Vec<Uuid>, AppError> {
    match query {
        Some(query) => reference::trades_matching_text(conn, query).await,
        None => Ok(Vec::new()),
    }
}

pub async fn list(
    State(state): State<AppState>,
    Query(raw): Query<search_input::RawQuery>,
) -> Result<Json<ListResponse>, AppError> {
    let ids = trade_ids(&state, raw.trade.as_deref()).await?;
    let mut request = search_input::parse(&raw, ids)?;

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    request.filters.query_trade_ids =
        query_trades(&mut conn, request.filters.query.as_deref()).await?;
    let mut page = search::list(
        &mut conn,
        &request.filters,
        request.sort,
        request.limit,
        request.cursor.as_ref(),
    )
    .await?;

    // Storage keys become URLs here, never in the row reader. Every read path
    // that serves a contractor has to do this or photos go out as bare keys.
    search::attach_photo_urls(&mut page.contractors, |key| state.store.url_for(key));

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
    /// Only the street line, not the whole address: it is a pin label, and the
    /// city and ZIP are already implied by where the pin is.
    #[serde(skip_serializing_if = "Option::is_none")]
    address_line1: Option<String>,
}

pub async fn map(
    State(state): State<AppState>,
    Query(raw): Query<search_input::RawQuery>,
) -> Result<Json<MapResponse>, AppError> {
    let ids = trade_ids(&state, raw.trade.as_deref()).await?;
    let mut request = search_input::parse(&raw, ids)?;

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    request.filters.query_trade_ids =
        query_trades(&mut conn, request.filters.query.as_deref()).await?;
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
                address_line1: c.address_line1,
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
    /// Third-party reviews, capped. Empty for a listing the enrichment load
    /// never reached, which is most of them.
    ///
    /// The totals live on the flattened contractor as `google_rating` and
    /// `google_review_count`, and the count is Google's own — normally larger
    /// than this array, which is a sample. A client that renders "N reviews"
    /// should use the count, not `reviews.len()`.
    reviews: Vec<reviews::PublicReview>,
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
    let mut one = match Uuid::parse_str(&id) {
        Ok(uuid) => search::find_public(&mut conn, uuid).await?,
        Err(_) => search::find_public_by_slug(&mut conn, &id).await?,
    }
    .map(|c| vec![c])
    .ok_or(AppError::NotFound)?;
    search::attach_photo_urls(&mut one, |key| state.store.url_for(key));
    let contractor = one.remove(0);

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

    let reviews =
        reviews::list_for_contractor(&mut conn, contractor.id, reviews::MAX_PER_CONTRACTOR).await?;

    Ok(Json(DetailResponse {
        contractor,
        verification_reason,
        license_data_as_of,
        verification,
        reviews,
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
    /// The claimant's own address. All four parts or none — a partial address
    /// is refused rather than merged with the licence's, because merging would
    /// geocode a building that exists nowhere.
    ///
    /// `Some(null)` clears it and the listing falls back to the licence
    /// address; absent leaves it alone. That distinction is why this is
    /// `Option<Option<_>>` rather than a flat option.
    #[serde(default, deserialize_with = "double_option")]
    pub owner_address: Option<Option<OwnerAddressRequest>>,
    #[serde(default, deserialize_with = "double_option")]
    pub google_review_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub yelp_url: Option<Option<String>>,
    /// Present only so a client that sends it gets a clear refusal instead of
    /// silently having it ignored — which would teach the client it worked.
    #[serde(default)]
    pub verified: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct OwnerAddressRequest {
    pub line1: String,
    pub city: String,
    pub state: String,
    pub postal_code: String,
}

/// Distinguish "absent" from "explicitly null".
///
/// serde collapses both to `None` on a plain `Option`, which would make
/// "leave my Yelp link alone" and "remove my Yelp link" the same request.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// Turn the wire's `Option<Option<T>>` into the repo's explicit `Edit`.
fn edit<T>(field: Option<Option<T>>) -> contractors::Edit<T> {
    match field {
        None => contractors::Edit::Unchanged,
        Some(None) => contractors::Edit::Cleared,
        Some(Some(value)) => contractors::Edit::Set(value),
    }
}

/// A link the contractor supplies about themselves.
///
/// Checked rather than trusted: this string is rendered as an `href` on a
/// public page, so `javascript:` and `data:` must not survive. Only http(s) is
/// accepted, and the host has to look like a host.
fn clean_link(value: String, field: &str) -> Result<String, AppError> {
    let trimmed = value.trim().to_owned();
    if trimmed.is_empty() {
        return Err(AppError::invalid(format!("{field} cannot be blank.")));
    }
    let lowered = trimmed.to_ascii_lowercase();
    if !(lowered.starts_with("https://") || lowered.starts_with("http://")) {
        return Err(AppError::invalid(format!(
            "{field} must start with https:// or http://"
        )));
    }
    // Cheap structural check rather than a URL parser: everything after the
    // scheme up to the first slash must contain a dot and no whitespace.
    let host = lowered
        .split_once("//")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .unwrap_or("");
    if host.is_empty() || !host.contains('.') || host.contains(char::is_whitespace) {
        return Err(AppError::invalid(format!("{field} is not a valid link.")));
    }
    if trimmed.chars().count() > 500 {
        return Err(AppError::invalid(format!("{field} is too long.")));
    }
    Ok(trimmed)
}

/// One part of an address, trimmed and bounded.
fn clean_address_part(value: &str, field: &str) -> Result<String, AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::invalid(format!("{field} cannot be blank.")));
    }
    if trimmed.chars().count() > 200 {
        return Err(AppError::invalid(format!("{field} is too long.")));
    }
    Ok(trimmed.to_owned())
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

    // Validated before the transaction opens, so a bad link is a 400 rather
    // than a rolled-back write.
    let owner_address = match body.owner_address {
        None => contractors::Edit::Unchanged,
        Some(None) => contractors::Edit::Cleared,
        Some(Some(a)) => contractors::Edit::Set(contractors::OwnerAddress {
            line1: clean_address_part(&a.line1, "Street address")?,
            city: clean_address_part(&a.city, "City")?,
            state: clean_address_part(&a.state, "State")?,
            postal_code: clean_address_part(&a.postal_code, "ZIP code")?,
        }),
    };
    let address_changed = !matches!(owner_address, contractors::Edit::Unchanged);

    let google_review_url = match edit(body.google_review_url) {
        contractors::Edit::Set(v) => contractors::Edit::Set(clean_link(v, "Google review link")?),
        other => other,
    };
    let yelp_url = match edit(body.yelp_url) {
        contractors::Edit::Set(v) => contractors::Edit::Set(clean_link(v, "Yelp link")?),
        other => other,
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
            owner_address,
            google_review_url,
            yelp_url,
        },
    )
    .await?;

    // Turning publication off has to take effect now, not at the next geocode.
    if visibility.is_some() {
        cm_domain::location::reapply(&mut tx, contractor_id).await?;
    }

    // A new address means a new pin. Queued inside the same transaction as the
    // write, so a rollback cannot leave a geocode job for an address that was
    // never saved.
    if address_changed {
        cm_domain::contractors::relocate_after_address_change(&mut tx, contractor_id).await?;
    }

    tx.commit().await.map_err(AppError::internal)?;

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    let mut found = search::find_public(&mut conn, contractor_id)
        .await?
        .map(|c| vec![c])
        .ok_or(AppError::NotFound)?;
    search::attach_photo_urls(&mut found, |key| state.store.url_for(key));
    Ok(Json(found.remove(0)))
}

/// Set the listing's profile photo. Multipart, claimant only.
pub async fn set_photo(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(contractor_id): Path<Uuid>,
    mut form: axum::extract::Multipart,
) -> Result<Json<cm_domain::contractors::ProfilePhoto>, AppError> {
    let mut bytes: Option<Vec<u8>> = None;

    while let Some(field) = form
        .next_field()
        .await
        .map_err(|error| AppError::invalid(format!("That upload could not be read: {error}")))?
    {
        if field.name() == Some("file") {
            bytes = Some(
                field
                    .bytes()
                    .await
                    .map_err(|error| {
                        AppError::invalid(format!("That upload could not be read: {error}"))
                    })?
                    .to_vec(),
            );
            break;
        }
    }

    let bytes = bytes.ok_or_else(|| AppError::invalid("Attach a photo in a \"file\" field."))?;

    let photo = cm_domain::contractors::set_photo(
        &state.pool,
        &state.store,
        state.auth.pepper(),
        caller.user.id,
        contractor_id,
        &bytes,
    )
    .await?;

    Ok(Json(photo))
}

pub async fn remove_photo(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(contractor_id): Path<Uuid>,
) -> Result<http::StatusCode, AppError> {
    cm_domain::contractors::remove_photo(&state.pool, &state.store, caller.user.id, contractor_id)
        .await?;
    Ok(http::StatusCode::NO_CONTENT)
}

/// A profile photo is one image, so the limit is lower than the job composer's
/// twelve megabytes — it is a logo or a van, not a set of site photographs.
const MAX_PHOTO_BYTES: usize = 8 * 1024 * 1024;

/// The photo routes, with the upload limit attached to them and nowhere else.
pub fn photo_routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/v1/contractors/{id}/photo",
        axum::routing::post(set_photo)
            .layer(axum::extract::DefaultBodyLimit::max(MAX_PHOTO_BYTES))
            .delete(remove_photo),
    )
}
