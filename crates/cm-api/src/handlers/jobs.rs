//! Jobs: posting, and the board everyone browses.
//!
//! The board has one projection and no notion of who is asking. Reads take no
//! session at all — not "a session that is ignored", but no extractor for one —
//! so there is no branch here that could serve the wrong caller the wrong shape.
//! What a job may reveal is decided in the schema, which has no address column
//! and no precise point; see `migrations/0017_jobs.sql`.
//!
//! Writes are the opposite: posting and closing are the homeowner's side, and
//! both check the account type before touching the database.

use crate::extract::{Context, CurrentUser, Json as ValidJson};
use crate::state::AppState;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use cm_core::AppError;
use cm_db::repo::jobs::{BuildType, JobStatus, JobTimeline, OwnerJob, Photo, PublicJob};
use cm_db::repo::{job_photos, jobs, reference};
use serde::Deserialize;
use uuid::Uuid;

/// The largest upload accepted, before decoding.
///
/// Applied to the photo route alone rather than the whole server, so a 12 MB
/// ceiling on images does not become a 12 MB ceiling on JSON bodies. A modern
/// phone photo is 3–6 MB, so this takes them without inviting anything else.
const MAX_UPLOAD_BYTES: usize = 12 * 1024 * 1024;

/// Every field is required, and each is a `String` rather than an
/// `Option<String>` so serde refuses a body with one missing before any of this
/// code runs. That is what lets the layers below read `None` as a deliberate
/// "Other" or "I'm not sure" rather than as an omission — see the header of
/// `migrations/0018_job_intake.sql`.
///
/// The escapes travel as sentinel strings: `trade: "other"` and
/// `budget: "unsure"`. A sentinel keeps the field present on the wire, which
/// keeps "the poster chose not to say" distinguishable from "the client forgot
/// to send it" — a distinction that vanishes the moment a field is optional.
#[derive(Debug, Deserialize)]
pub struct PostJobRequest {
    pub title: String,
    pub description: String,
    /// A trade slug, or the literal `"other"`.
    pub trade: String,
    pub build_type: String,
    pub job_size: String,
    pub postal_code: String,
    pub timeline: String,
    /// A range, or the literal `"unsure"`.
    pub budget: BudgetRequest,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BudgetRequest {
    /// `{"min_cents": 800000, "max_cents": 1500000}`
    Range { min_cents: i64, max_cents: i64 },
    /// `"unsure"`, and nothing else.
    Unsure(String),
}

impl BudgetRequest {
    fn parse(self) -> Result<Option<(i64, i64)>, AppError> {
        match self {
            Self::Range { min_cents, max_cents } => Ok(Some((min_cents, max_cents))),
            Self::Unsure(word) if word.trim().eq_ignore_ascii_case("unsure") => Ok(None),
            Self::Unsure(other) => Err(AppError::invalid(format!(
                "Budget must be a range or \"unsure\"; got \"{other}\"."
            ))),
        }
    }
}

/// The sentinel a caller sends for "Other / not listed".
const TRADE_OTHER: &str = "other";

#[derive(Debug, serde::Serialize)]
pub struct ListResponse {
    pub jobs: Vec<PublicJob>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Filters that were not understood and were dropped. Naming them beats a
    /// 400: an unknown trade slug should narrow nothing, not fail the page.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_filters: Vec<String>,
}

pub async fn post_job(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    ValidJson(body): ValidJson<PostJobRequest>,
) -> Result<(StatusCode, Json<OwnerJob>), AppError> {
    // Posting work is the homeowner's side. The database trigger refuses it
    // too; this is here so the caller gets a 403 rather than a 500.
    if !caller.user.account_type.may_hire() {
        // Only a homeowner account can post a job.
        return Err(AppError::Forbidden);
    }

    let timeline = JobTimeline::parse_request(body.timeline.trim())?;
    let build_type = BuildType::parse_request(body.build_type.trim())?;

    // "other" is a choice, not a slug: it reaches the domain as None, which is
    // what the schema records.
    let trade = body.trade.trim();
    let trade_slug = match trade {
        "" => {
            return Err(AppError::invalid(
                "Choose a job type, or \"Other / not listed\".",
            ))
        }
        t if t.eq_ignore_ascii_case(TRADE_OTHER) => None,
        t => Some(t.to_owned()),
    };

    let job = cm_domain::jobs::post(
        &state.pool,
        state.auth.pepper(),
        caller.user.id,
        cm_domain::jobs::PostJob {
            title: body.title,
            description: body.description,
            trade_slug,
            build_type,
            job_size: body.job_size,
            postal_code: body.postal_code,
            budget: body.budget.parse()?,
            timeline,
        },
        context.request_id,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(job)))
}

pub async fn list(
    State(state): State<AppState>,
    Query(raw): Query<cm_domain::jobs::RawQuery>,
) -> Result<Json<ListResponse>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;

    let trade_ids = match raw
        .trade
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(list) => {
            let slugs: Vec<String> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            reference::trade_ids_for_slugs(&mut conn, &slugs).await?
        }
        None => Vec::new(),
    };

    let query = cm_domain::jobs::parse(&raw, trade_ids)?;
    let page = jobs::list(
        &mut conn,
        &query.filters,
        query.limit,
        query.cursor.as_ref(),
    )
    .await?;

    Ok(Json(ListResponse {
        jobs: page.jobs,
        next_cursor: page
            .next_cursor
            .as_ref()
            .map(cm_domain::jobs::encode_cursor),
        ignored_filters: query.ignored,
    }))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicJob>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    let job = jobs::find(&mut conn, id).await?.ok_or(AppError::NotFound)?;

    let mut one = [job];
    let photos = job_photos::for_jobs(&mut conn, &[id]).await?;
    jobs::attach_photos(&mut one, photos, |key| state.store.url_for(key));

    let [job] = one;
    Ok(Json(job))
}

/// Attach a photo. Multipart, one file per request.
///
/// One at a time rather than a batch: a batch fails as a unit, and losing four
/// good photos because the fifth was a screenshot of a PDF is a worse experience
/// than five requests. The composer uploads them in sequence and reports each.
pub async fn add_photo(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<Uuid>,
    mut form: Multipart,
) -> Result<(StatusCode, Json<Photo>), AppError> {
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

    let photo = cm_domain::jobs::attach_photo(
        &state.pool,
        &state.store,
        state.auth.pepper(),
        caller.user.id,
        id,
        &bytes,
        context.request_id,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(photo)))
}

pub async fn remove_photo(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    Path((id, photo_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    cm_domain::jobs::remove_photo(
        &state.pool,
        &state.store,
        caller.user.id,
        id,
        photo_id,
        context.request_id,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The photo routes, with the upload limit attached to them and nowhere else.
pub fn photo_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/jobs/{id}/photos",
            axum::routing::post(add_photo).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route(
            "/v1/jobs/{id}/photos/{photo_id}",
            axum::routing::delete(remove_photo),
        )
}

/// The caller's own posts, in every state. No account-type check: a contractor
/// account simply has none, and an empty list is the honest answer.
pub async fn mine(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
) -> Result<Json<Vec<OwnerJob>>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    Ok(Json(jobs::for_poster(&mut conn, caller.user.id).await?))
}

#[derive(Debug, Deserialize)]
pub struct CloseRequest {
    /// "closed" (the work is handled) or "cancelled" (never mind).
    #[serde(default)]
    pub status: Option<String>,
}

pub async fn close(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<Uuid>,
    ValidJson(body): ValidJson<CloseRequest>,
) -> Result<StatusCode, AppError> {
    let status = match body
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some("closed") | None => JobStatus::Closed,
        Some("cancelled") => JobStatus::Cancelled,
        Some(other) => {
            return Err(AppError::invalid(format!(
                "unknown status \"{other}\"; expected closed or cancelled"
            )))
        }
    };

    cm_domain::jobs::close(
        &state.pool,
        &state.store,
        caller.user.id,
        id,
        status,
        context.request_id,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Kept in one function so it is obvious at the call site which job routes an
/// anonymous caller reaches.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/jobs", axum::routing::get(list))
        .route("/v1/jobs/{id}", axum::routing::get(detail))
}
