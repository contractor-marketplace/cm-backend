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
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use cm_core::AppError;
use cm_db::repo::jobs::{JobStatus, JobTimeline, OwnerJob, PublicJob};
use cm_db::repo::{jobs, reference};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct PostJobRequest {
    pub title: String,
    pub description: String,
    pub trade: Option<String>,
    pub postal_code: Option<String>,
    pub budget_min_cents: Option<i64>,
    pub budget_max_cents: Option<i64>,
    pub timeline: Option<String>,
}

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

    let timeline = body
        .timeline
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(JobTimeline::parse_request)
        .transpose()?;

    let job = cm_domain::jobs::post(
        &state.pool,
        state.auth.pepper(),
        caller.user.id,
        cm_domain::jobs::PostJob {
            title: body.title,
            description: body.description,
            trade_slug: body.trade,
            postal_code: body.postal_code,
            budget_min_cents: body.budget_min_cents,
            budget_max_cents: body.budget_max_cents,
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
    jobs::find(&mut conn, id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
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

    cm_domain::jobs::close(&state.pool, caller.user.id, id, status, context.request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Kept in one function so it is obvious at the call site which job routes an
/// anonymous caller reaches.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/jobs", axum::routing::get(list))
        .route("/v1/jobs/{id}", axum::routing::get(detail))
}
