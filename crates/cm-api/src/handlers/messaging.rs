//! Direct messaging, blocking and reporting.

use crate::extract::{Context, CurrentUser, Json as ValidJson};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
use cm_core::AppError;
use cm_db::repo::messaging::{Conversation, ConversationSummary, Message, Report};
use cm_db::repo::users::Role;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub contractor_id: Uuid,
}

pub async fn start(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    ValidJson(body): ValidJson<StartRequest>,
) -> Result<(StatusCode, Json<Conversation>), AppError> {
    // Starting a conversation is how a homeowner hires. A contractor account
    // replies inside a conversation a homeowner opened, and never opens one —
    // an account is one side of the marketplace or the other, never both.
    if !caller.user.account_type.may_hire() {
        // Only a homeowner account can start a conversation.
        return Err(AppError::Forbidden);
    }

    let conversation = cm_domain::messaging::start_with_contractor(
        &state.pool,
        state.auth.pepper(),
        caller.user.id,
        body.contractor_id,
        context.request_id,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(conversation)))
}

pub async fn list(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
) -> Result<Json<Vec<ConversationSummary>>, AppError> {
    Ok(Json(
        cm_domain::messaging::conversations(&state.pool, caller.user.id, 100).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct PollQuery {
    pub after_seq: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PollResponse {
    messages: Vec<Message>,
    /// Where to resume. Equal to `after_seq` when nothing new arrived.
    next_seq: i64,
    /// How long a client should wait before asking again.
    poll_after_secs: u64,
}

pub async fn poll(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(conversation_id): Path<Uuid>,
    Query(query): Query<PollQuery>,
) -> Result<Json<PollResponse>, AppError> {
    let after = query.after_seq.unwrap_or(0).max(0);
    let messages = cm_domain::messaging::poll(
        &state.pool,
        conversation_id,
        caller.user.id,
        after,
        query.limit.unwrap_or(cm_domain::messaging::MAX_PAGE),
    )
    .await?;

    let next_seq = messages.last().map(|m| m.seq).unwrap_or(after);

    Ok(Json(PollResponse {
        messages,
        next_seq,
        poll_after_secs: cm_domain::messaging::POLL_INTERVAL_SECS,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SendRequest {
    pub body: String,
}

pub async fn send(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(conversation_id): Path<Uuid>,
    ValidJson(body): ValidJson<SendRequest>,
) -> Result<(StatusCode, Json<Message>), AppError> {
    let message = cm_domain::messaging::send(
        &state.pool,
        state.auth.pepper(),
        conversation_id,
        caller.user.id,
        &body.body,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(message)))
}

#[derive(Debug, Deserialize)]
pub struct ReadRequest {
    pub up_to_seq: i64,
}

pub async fn mark_read(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(conversation_id): Path<Uuid>,
    ValidJson(body): ValidJson<ReadRequest>,
) -> Result<StatusCode, AppError> {
    cm_domain::messaging::mark_read(
        &state.pool,
        conversation_id,
        caller.user.id,
        body.up_to_seq.max(0),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct BlockRequest {
    pub reason: Option<String>,
}

pub async fn block(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    Path(user_id): Path<Uuid>,
    ValidJson(body): ValidJson<BlockRequest>,
) -> Result<StatusCode, AppError> {
    cm_domain::messaging::block(
        &state.pool,
        caller.user.id,
        user_id,
        body.reason,
        context.request_id,
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn unblock(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    cm_domain::messaging::unblock(&state.pool, caller.user.id, user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn blocked(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
) -> Result<Json<Vec<Uuid>>, AppError> {
    Ok(Json(
        cm_domain::messaging::blocked_users(&state.pool, caller.user.id).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct ReportRequest {
    pub conversation_id: Uuid,
    pub message_id: Option<Uuid>,
    pub reason: String,
    pub detail: Option<String>,
}

pub async fn report(
    State(state): State<AppState>,
    Context(context): Context,
    CurrentUser(caller): CurrentUser,
    ValidJson(body): ValidJson<ReportRequest>,
) -> Result<(StatusCode, Json<Report>), AppError> {
    let report = cm_domain::messaging::report(
        &state.pool,
        state.auth.pepper(),
        cm_domain::messaging::NewReport {
            reporter: caller.user.id,
            conversation_id: body.conversation_id,
            message_id: body.message_id,
            reason: body.reason,
            detail: body.detail,
            request_id: context.request_id,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(report)))
}

pub async fn open_reports(
    State(state): State<AppState>,
    CurrentUser(caller): CurrentUser,
) -> Result<Json<Vec<Report>>, AppError> {
    if !caller.has_role(Role::Admin) && !caller.has_role(Role::Moderator) {
        return Err(AppError::Forbidden);
    }

    let mut conn = state.pool.acquire().await.map_err(AppError::internal)?;
    Ok(Json(
        cm_db::repo::messaging::open_reports(&mut conn, 100).await?,
    ))
}
