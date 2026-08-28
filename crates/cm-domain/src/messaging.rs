//! Direct messaging rules.
//!
//! Messaging is the surface most exposed to abuse, so the gate is deliberately
//! narrow: you may only open a conversation with a contractor that has been
//! **claimed** and has **opted in**, you may not do it more than a few times a
//! day, and a block stops everything in both directions.

use cm_auth::ratelimit;
use cm_core::{AppError, Secret};
use cm_db::repo::audit::{ActorKind, AuditEvent};
use cm_db::repo::messaging::{self, Conversation, ConversationSummary, Message, Report};
use cm_db::repo::{audit, contractors};
use cm_db::PgPool;
use uuid::Uuid;

/// Ceiling on one page of messages.
pub const MAX_PAGE: i64 = 100;
/// What the client is told to wait between polls. Advisory, but it keeps a
/// well-behaved client from turning a poll into a busy loop.
pub const POLL_INTERVAL_SECS: u64 = 3;

fn conversation_create_policy() -> ratelimit::Policy {
    ratelimit::Policy {
        name: "conversation_create:user",
        limit: 10,
        window: chrono::Duration::days(1),
    }
}

fn message_send_policy() -> ratelimit::Policy {
    ratelimit::Policy {
        name: "message_send:user",
        limit: 60,
        window: chrono::Duration::hours(1),
    }
}

fn report_policy() -> ratelimit::Policy {
    ratelimit::Policy {
        name: "report:user",
        limit: 20,
        window: chrono::Duration::days(1),
    }
}

/// Open a conversation with a contractor.
pub async fn start_with_contractor(
    pool: &PgPool,
    pepper: &Secret<String>,
    initiator: Uuid,
    contractor_id: Uuid,
    request_id: Option<String>,
) -> Result<Conversation, AppError> {
    ratelimit::enforce(
        pool,
        pepper,
        conversation_create_policy(),
        &initiator.to_string(),
        chrono::Utc::now(),
    )
    .await?;

    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    let target = contractors::messaging_target(&mut tx, contractor_id)
        .await?
        .ok_or(AppError::NotFound)?;

    // An unclaimed listing has nobody behind it to receive the message.
    let Some(owner) = target.claimed_by_user_id else {
        return Err(AppError::Forbidden);
    };
    if !target.accepts_dm {
        return Err(AppError::Forbidden);
    }
    if owner == initiator {
        return Err(AppError::invalid(
            "You cannot start a conversation with your own listing.",
        ));
    }
    if messaging::blocked_either_way(&mut tx, initiator, owner).await? {
        // Deliberately the same answer as "does not accept messages": telling
        // someone they have been blocked invites them to work around it.
        return Err(AppError::Forbidden);
    }

    let (conversation, created) =
        messaging::find_or_create_dm(&mut tx, initiator, owner, Some(contractor_id)).await?;

    if created {
        audit::record(
            &mut tx,
            AuditEvent::new("conversation.created", "conversations")
                .actor(ActorKind::User, Some(initiator))
                .subject(conversation.id)
                .data(serde_json::json!({ "contractor_id": contractor_id }))
                .request_id(request_id),
        )
        .await?;
    }

    tx.commit().await.map_err(AppError::internal)?;
    Ok(conversation)
}

/// Send a message into an existing conversation.
pub async fn send(
    pool: &PgPool,
    pepper: &Secret<String>,
    conversation_id: Uuid,
    sender: Uuid,
    body: &str,
) -> Result<Message, AppError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(AppError::invalid("A message cannot be empty."));
    }
    if body.chars().count() > 4000 {
        return Err(AppError::invalid(
            "A message must be under 4000 characters.",
        ));
    }

    ratelimit::enforce(
        pool,
        pepper,
        message_send_policy(),
        &sender.to_string(),
        chrono::Utc::now(),
    )
    .await?;

    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    // 404, not 403: a non-participant must not learn that the conversation
    // exists at all.
    if !messaging::is_participant(&mut tx, conversation_id, sender).await? {
        return Err(AppError::NotFound);
    }

    if let Some(other) = messaging::counterpart(&mut tx, conversation_id, sender).await? {
        if messaging::blocked_either_way(&mut tx, sender, other).await? {
            return Err(AppError::Forbidden);
        }
    }

    let message = messaging::send(&mut tx, conversation_id, sender, body).await?;
    tx.commit().await.map_err(AppError::internal)?;

    Ok(message)
}

pub async fn conversations(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<ConversationSummary>, AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    messaging::list_for_user(&mut conn, user_id, limit).await
}

/// Poll a conversation. The cursor contract a WebSocket would later satisfy
/// without a client rewrite.
pub async fn poll(
    pool: &PgPool,
    conversation_id: Uuid,
    user_id: Uuid,
    after_seq: i64,
    limit: i64,
) -> Result<Vec<Message>, AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;

    if !messaging::is_participant(&mut conn, conversation_id, user_id).await? {
        return Err(AppError::NotFound);
    }

    messaging::after_seq(
        &mut conn,
        conversation_id,
        after_seq,
        limit.clamp(1, MAX_PAGE),
    )
    .await
}

/// Remove a message you sent.
///
/// Soft, always. The row keeps its `seq` and its body becomes `[removed]`, for
/// two reasons: a hole in the sequence would make the poll cursor ambiguous —
/// a client could not tell "message 7 was deleted" from "message 7 has not
/// arrived yet" — and a report is investigated against the conversation, so a
/// hard delete would let someone erase the evidence against them after the
/// other party had already reported it.
///
/// Only the sender. Not the recipient, and not a moderator: taking somebody
/// else's words off the record is a different action from retracting your own,
/// and it is not one this product offers.
pub async fn delete_message(
    pool: &PgPool,
    conversation_id: Uuid,
    message_id: Uuid,
    sender: Uuid,
) -> Result<(), AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;

    // 404 before anything else, so a non-participant cannot probe for the
    // existence of a conversation by trying to delete out of it.
    if !messaging::is_participant(&mut conn, conversation_id, sender).await? {
        return Err(AppError::NotFound);
    }

    // The repo puts sender and conversation in the WHERE clause, so a
    // participant deleting somebody else's message matches no row and is told
    // the same thing as a stranger.
    if !messaging::soft_delete(&mut conn, conversation_id, message_id, sender).await? {
        return Err(AppError::NotFound);
    }

    Ok(())
}

pub async fn mark_read(
    pool: &PgPool,
    conversation_id: Uuid,
    user_id: Uuid,
    up_to_seq: i64,
) -> Result<(), AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;

    if !messaging::is_participant(&mut conn, conversation_id, user_id).await? {
        return Err(AppError::NotFound);
    }

    messaging::mark_read(&mut conn, conversation_id, user_id, up_to_seq).await
}

/// Block someone.
///
/// Existing history stays readable to both: deleting a conversation on block
/// would destroy the evidence a report depends on.
pub async fn block(
    pool: &PgPool,
    blocker: Uuid,
    blocked: Uuid,
    reason: Option<String>,
    request_id: Option<String>,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await.map_err(AppError::internal)?;
    messaging::block(&mut tx, blocker, blocked, reason.as_deref()).await?;
    audit::record(
        &mut tx,
        AuditEvent::new("user.blocked", "user_blocks")
            .actor(ActorKind::User, Some(blocker))
            .subject(blocked)
            .request_id(request_id),
    )
    .await?;
    tx.commit().await.map_err(AppError::internal)?;
    Ok(())
}

pub async fn unblock(pool: &PgPool, blocker: Uuid, blocked: Uuid) -> Result<bool, AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    messaging::unblock(&mut conn, blocker, blocked).await
}

pub async fn blocked_users(pool: &PgPool, blocker: Uuid) -> Result<Vec<Uuid>, AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    messaging::blocks_by(&mut conn, blocker).await
}

/// Report a conversation or a message in it.
///
/// The reported party is not notified: telling someone they have been reported
/// is how a report becomes a reason to retaliate.
pub struct NewReport {
    pub reporter: Uuid,
    pub conversation_id: Uuid,
    pub message_id: Option<Uuid>,
    pub reason: String,
    pub detail: Option<String>,
    pub request_id: Option<String>,
}

pub async fn report(
    pool: &PgPool,
    pepper: &Secret<String>,
    new: NewReport,
) -> Result<Report, AppError> {
    let NewReport {
        reporter,
        conversation_id,
        message_id,
        reason,
        detail,
        request_id,
    } = new;
    let reason = reason.as_str();
    const REASONS: [&str; 5] = [
        "spam",
        "harassment",
        "scam",
        "off_platform_payment",
        "other",
    ];
    if !REASONS.contains(&reason) {
        return Err(AppError::invalid(format!(
            "reason must be one of {}",
            REASONS.join(", ")
        )));
    }

    ratelimit::enforce(
        pool,
        pepper,
        report_policy(),
        &reporter.to_string(),
        chrono::Utc::now(),
    )
    .await?;

    let mut tx = pool.begin().await.map_err(AppError::internal)?;
    if !messaging::is_participant(&mut tx, conversation_id, reporter).await? {
        return Err(AppError::NotFound);
    }

    let report = messaging::report(
        &mut tx,
        reporter,
        conversation_id,
        message_id,
        reason,
        detail.as_deref(),
    )
    .await?;

    audit::record(
        &mut tx,
        AuditEvent::new("message.reported", "message_reports")
            .actor(ActorKind::User, Some(reporter))
            .subject(report.id)
            .data(serde_json::json!({ "reason": reason }))
            .request_id(request_id),
    )
    .await?;
    tx.commit().await.map_err(AppError::internal)?;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_messaging_limit_is_bounded_and_distinct() {
        let policies = [
            conversation_create_policy(),
            message_send_policy(),
            report_policy(),
        ];

        for policy in policies {
            assert!(policy.limit > 0, "{}", policy.name);
            assert!(policy.window > chrono::Duration::zero(), "{}", policy.name);
        }

        let names: std::collections::HashSet<&str> =
            policies.iter().map(|policy| policy.name).collect();
        assert_eq!(names.len(), policies.len());
    }
}
