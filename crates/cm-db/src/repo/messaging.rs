//! Direct messaging, blocking and reporting.
//!
//! Two mechanisms carry the correctness here.
//!
//! **One conversation per pair.** The pair is stored canonically ordered and
//! covered by a unique index, so two simultaneous "start a chat" requests
//! return the same conversation rather than two.
//!
//! **Gapless ordering.** Each message takes its sequence from a counter on the
//! conversation row, incremented inside the insert's transaction. That row lock
//! serialises sends per conversation — exactly the granularity DM traffic
//! needs — and makes `WHERE seq > $cursor` an exact poll. Polling on a
//! timestamp would lose messages, because a transaction that starts earlier can
//! commit later and land behind a cursor the client has already passed.

use chrono::{DateTime, Utc};
use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Conversation {
    pub id: Uuid,
    pub contractor_id: Option<Uuid>,
    pub last_seq: i64,
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Conversation {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            contractor_id: row.try_get("contractor_id")?,
            last_seq: row.try_get("last_seq")?,
            last_message_at: row.try_get("last_message_at")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Message {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
    pub sender_user_id: Uuid,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub deleted: bool,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Message {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let deleted_at: Option<DateTime<Utc>> = row.try_get("deleted_at")?;
        let body: String = row.try_get("body")?;

        Ok(Self {
            id: row.try_get("id")?,
            conversation_id: row.try_get("conversation_id")?,
            seq: row.try_get("seq")?,
            sender_user_id: row.try_get("sender_user_id")?,
            // A removed message keeps its place in the sequence — a hole would
            // make the poll cursor ambiguous — but not its content.
            body: if deleted_at.is_some() {
                "[removed]".to_owned()
            } else {
                body
            },
            created_at: row.try_get("created_at")?,
            deleted: deleted_at.is_some(),
        })
    }
}

/// Canonical ordering for a pair, so (a,b) and (b,a) are one conversation.
fn ordered(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Find the pair's conversation, or create it.
///
/// `ON CONFLICT DO NOTHING` followed by a re-select: two simultaneous callers
/// therefore receive the same row instead of racing to insert two.
pub async fn find_or_create_dm(
    conn: &mut PgConnection,
    initiator: Uuid,
    recipient: Uuid,
    contractor_id: Option<Uuid>,
) -> Result<(Conversation, bool), AppError> {
    if initiator == recipient {
        return Err(AppError::invalid(
            "You cannot start a conversation with yourself.",
        ));
    }
    let (lo, hi) = ordered(initiator, recipient);

    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO conversations (id, kind, contractor_id, dm_lo, dm_hi, created_by) \
         VALUES ($1, 'dm', $2, $3, $4, $5) \
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(new_id())
    .bind(contractor_id)
    .bind(lo)
    .bind(hi)
    .bind(initiator)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    let conversation: Conversation = sqlx::query_as(
        "SELECT id, contractor_id, last_seq, last_message_at, created_at \
           FROM conversations WHERE kind = 'dm' AND dm_lo = $1 AND dm_hi = $2",
    )
    .bind(lo)
    .bind(hi)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    if inserted.is_some() {
        for (user_id, role) in [(initiator, "initiator"), (recipient, "recipient")] {
            sqlx::query(
                "INSERT INTO conversation_participants (conversation_id, user_id, role) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(conversation.id)
            .bind(user_id)
            .bind(role)
            .execute(&mut *conn)
            .await
            .map_err(AppError::internal)?;
        }
    }

    Ok((conversation, inserted.is_some()))
}

pub async fn is_participant(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_participants \
                         WHERE conversation_id = $1 AND user_id = $2)",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// The other party in a two-person conversation.
pub async fn counterpart(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    user_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    sqlx::query_scalar(
        "SELECT user_id FROM conversation_participants \
          WHERE conversation_id = $1 AND user_id <> $2 LIMIT 1",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// A conversation with the caller's unread count.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationSummary {
    pub id: Uuid,
    pub contractor_id: Option<Uuid>,
    pub counterpart_user_id: Option<Uuid>,
    pub counterpart_name: Option<String>,
    pub last_seq: i64,
    pub last_read_seq: i64,
    pub unread: i64,
    pub last_message_at: Option<DateTime<Utc>>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for ConversationSummary {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            contractor_id: row.try_get("contractor_id")?,
            counterpart_user_id: row.try_get("counterpart_user_id")?,
            counterpart_name: row.try_get("counterpart_name")?,
            last_seq: row.try_get("last_seq")?,
            last_read_seq: row.try_get("last_read_seq")?,
            unread: row.try_get("unread")?,
            last_message_at: row.try_get("last_message_at")?,
        })
    }
}

pub async fn list_for_user(
    conn: &mut PgConnection,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<ConversationSummary>, AppError> {
    sqlx::query_as(
        "SELECT c.id, c.contractor_id, other.user_id AS counterpart_user_id, \
                u.display_name AS counterpart_name, c.last_seq, p.last_read_seq, \
                GREATEST(c.last_seq - p.last_read_seq, 0) AS unread, c.last_message_at \
           FROM conversation_participants p \
           JOIN conversations c ON c.id = p.conversation_id \
           LEFT JOIN conversation_participants other \
                  ON other.conversation_id = c.id AND other.user_id <> p.user_id \
           LEFT JOIN users u ON u.id = other.user_id \
          WHERE p.user_id = $1 \
          ORDER BY c.last_message_at DESC NULLS LAST, c.id \
          LIMIT $2",
    )
    .bind(user_id)
    .bind(limit.clamp(1, 100))
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Append a message, taking the next sequence under the conversation's lock.
pub async fn send(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    sender: Uuid,
    body: &str,
) -> Result<Message, AppError> {
    // The row lock this update takes is what serialises sends within one
    // conversation, so the sequence has no gaps and no duplicates.
    let seq: i64 = sqlx::query_scalar(
        "UPDATE conversations \
            SET last_seq = last_seq + 1, last_message_at = now(), updated_at = now() \
          WHERE id = $1 RETURNING last_seq",
    )
    .bind(conversation_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    sqlx::query_as(
        "INSERT INTO messages (id, conversation_id, seq, sender_user_id, body) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, conversation_id, seq, sender_user_id, body, created_at, deleted_at",
    )
    .bind(new_id())
    .bind(conversation_id)
    .bind(seq)
    .bind(sender)
    .bind(body)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Messages after a cursor. The contract a WebSocket push would later satisfy
/// unchanged.
pub async fn after_seq(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    after: i64,
    limit: i64,
) -> Result<Vec<Message>, AppError> {
    sqlx::query_as(
        "SELECT id, conversation_id, seq, sender_user_id, body, created_at, deleted_at \
           FROM messages WHERE conversation_id = $1 AND seq > $2 ORDER BY seq LIMIT $3",
    )
    .bind(conversation_id)
    .bind(after)
    .bind(limit.clamp(1, 100))
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn mark_read(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    user_id: Uuid,
    up_to_seq: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE conversation_participants \
            SET last_read_seq = GREATEST(last_read_seq, $3), updated_at = now() \
          WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(user_id)
    .bind(up_to_seq)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Soft-delete a message. The row stays so the sequence has no hole.
pub async fn soft_delete(
    conn: &mut PgConnection,
    message_id: Uuid,
    by: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE messages SET deleted_at = now(), deleted_by = $2, updated_at = now() \
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(message_id)
    .bind(by)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

// ── blocking ────────────────────────────────────────────────────────────────

pub async fn block(
    conn: &mut PgConnection,
    blocker: Uuid,
    blocked: Uuid,
    reason: Option<&str>,
) -> Result<(), AppError> {
    if blocker == blocked {
        return Err(AppError::invalid("You cannot block yourself."));
    }

    sqlx::query(
        "INSERT INTO user_blocks (blocker_user_id, blocked_user_id, reason) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (blocker_user_id, blocked_user_id) DO UPDATE \
             SET reason = EXCLUDED.reason, updated_at = now()",
    )
    .bind(blocker)
    .bind(blocked)
    .bind(reason)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

pub async fn unblock(
    conn: &mut PgConnection,
    blocker: Uuid,
    blocked: Uuid,
) -> Result<bool, AppError> {
    let result =
        sqlx::query("DELETE FROM user_blocks WHERE blocker_user_id = $1 AND blocked_user_id = $2")
            .bind(blocker)
            .bind(blocked)
            .execute(&mut *conn)
            .await
            .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

/// A block stops messages in both directions, so both orderings are checked.
pub async fn blocked_either_way(
    conn: &mut PgConnection,
    a: Uuid,
    b: Uuid,
) -> Result<bool, AppError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM user_blocks \
                         WHERE (blocker_user_id = $1 AND blocked_user_id = $2) \
                            OR (blocker_user_id = $2 AND blocked_user_id = $1))",
    )
    .bind(a)
    .bind(b)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn blocks_by(conn: &mut PgConnection, blocker: Uuid) -> Result<Vec<Uuid>, AppError> {
    sqlx::query_scalar(
        "SELECT blocked_user_id FROM user_blocks WHERE blocker_user_id = $1 ORDER BY created_at",
    )
    .bind(blocker)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

// ── reporting ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub message_id: Option<Uuid>,
    pub reason: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Report {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            conversation_id: row.try_get("conversation_id")?,
            message_id: row.try_get("message_id")?,
            reason: row.try_get("reason")?,
            status: row.try_get("status")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

pub async fn report(
    conn: &mut PgConnection,
    reporter: Uuid,
    conversation_id: Uuid,
    message_id: Option<Uuid>,
    reason: &str,
    detail: Option<&str>,
) -> Result<Report, AppError> {
    sqlx::query_as(
        "INSERT INTO message_reports \
             (id, reporter_user_id, conversation_id, message_id, reason, detail) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, conversation_id, message_id, reason, status, created_at",
    )
    .bind(new_id())
    .bind(reporter)
    .bind(conversation_id)
    .bind(message_id)
    .bind(reason)
    .bind(detail)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db)
            if db.constraint() == Some("message_reports_one_per_reporter_message") =>
        {
            AppError::conflict("You have already reported that message.")
        }
        _ => AppError::internal(error),
    })
}

pub async fn open_reports(conn: &mut PgConnection, limit: i64) -> Result<Vec<Report>, AppError> {
    sqlx::query_as(
        "SELECT id, conversation_id, message_id, reason, status, created_at \
           FROM message_reports WHERE status = 'open' ORDER BY created_at LIMIT $1",
    )
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn resolve_report(
    conn: &mut PgConnection,
    report_id: Uuid,
    status: &str,
    reviewer: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE message_reports \
            SET status = $2, reviewed_at = now(), reviewed_by = $3, updated_at = now() \
          WHERE id = $1 AND status IN ('open', 'reviewing')",
    )
    .bind(report_id)
    .bind(status)
    .bind(reviewer)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}
