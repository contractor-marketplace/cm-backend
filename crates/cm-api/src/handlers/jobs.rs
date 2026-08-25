//! Jobs: posting, and the board contractors browse.
//!
//! The tiering decision lives here and nowhere else: which of the three
//! projections in `cm_db::repo::jobs` a request gets. Everything below this
//! point is already narrowed, so a handler bug can pick the wrong tier but
//! cannot invent a field the query never selected.

use crate::extract::{Context, CurrentUser, Json as ValidJson, OptionalUser};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use cm_core::AppError;
use cm_db::repo::jobs::{
    ContractorJob, JobStatus, JobTimeline, OwnerJob, PublicJob,
};
use cm_db::repo::{jobs, reference, users::AccountType};
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

/// Two shapes, never merged into one with optional fields.
///
/// An `Option<String> description` that happens to be `None` for anonymous
/// callers is one careless `unwrap_or_default` away from leaking; two variants
/// make the tier visible at every call site.
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub enum JobView {
    ForContractor(ContractorJob),
    Public(PublicJob),
}

#[derive(Debug, serde::Serialize)]
pub struct ListResponse {
    pub jobs: Vec<JobView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_filters: Vec<String>,
    /// So a client can tell "you are seeing the redacted view" from "there is
    /// nothing more to see", without guessing from absent fields.
    pub detail_visible: bool,
}

/// Whether this caller gets the contractor projection.
///
/// A signed-out visitor and a signed-in homeowner both get the public view. The
/// extra detail is for the side of the marketplace that acts on it.
fn sees_detail(caller: &OptionalUser) -> bool {
    caller
        .0
        .as_ref()
        .is_some_and(|auth| auth.user.account_type == AccountType::Contractor)
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
    caller: OptionalUser,
    Query(raw): Query<cm_domain::jobs::RawQuery>,
) -> Result<Json<ListResponse>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;

    let trade_ids = match raw.trade.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
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
    let detail_visible = sees_detail(&caller);

    let (jobs, next_cursor) = if detail_visible {
        let page =
            jobs::list_for_contractor(&mut conn, &query.filters, query.limit, query.cursor.as_ref())
                .await?;
        (
            page.jobs.into_iter().map(JobView::ForContractor).collect(),
            page.next_cursor,
        )
    } else {
        let page =
            jobs::list_public(&mut conn, &query.filters, query.limit, query.cursor.as_ref()).await?;
        (
            page.jobs.into_iter().map(JobView::Public).collect(),
            page.next_cursor,
        )
    };

    Ok(Json(ListResponse {
        jobs,
        next_cursor: next_cursor
            .as_ref()
            .map(cm_domain::jobs::encode_cursor),
        ignored_filters: query.ignored,
        detail_visible,
    }))
}

#[derive(Debug, serde::Serialize)]
pub struct DetailResponse {
    #[serde(flatten)]
    pub job: JobView,
    pub detail_visible: bool,
}

pub async fn detail(
    State(state): State<AppState>,
    caller: OptionalUser,
    Path(id): Path<Uuid>,
) -> Result<Json<DetailResponse>, AppError> {
    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    let detail_visible = sees_detail(&caller);

    let job = if detail_visible {
        jobs::find_for_contractor(&mut conn, id)
            .await?
            .map(JobView::ForContractor)
    } else {
        jobs::find_public(&mut conn, id).await?.map(JobView::Public)
    }
    .ok_or(AppError::NotFound)?;

    Ok(Json(DetailResponse {
        job,
        detail_visible,
    }))
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
    let status = match body.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
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

/// The read-only half, mounted on the public router behind the optional-session
/// layer. Kept in one function so it is obvious at the call site that these are
/// the routes anonymous callers reach.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/jobs", axum::routing::get(list))
        .route("/v1/jobs/{id}", axum::routing::get(detail))
}
