//! The append-only audit log.

use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

/// Who performed the action. Mirrors the `audit_log.actor_kind` CHECK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    User,
    System,
    Importer,
    Admin,
}

impl ActorKind {
    pub const ALL: [Self; 4] = [Self::User, Self::System, Self::Importer, Self::Admin];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::Importer => "importer",
            Self::Admin => "admin",
        }
    }
}

/// One event, ready to be written.
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub actor_user_id: Option<Uuid>,
    pub actor_kind: ActorKind,
    pub action: &'static str,
    pub subject_table: &'static str,
    pub subject_id: Option<Uuid>,
    pub data: serde_json::Value,
    pub request_id: Option<String>,
    pub ip_hash: Option<Vec<u8>>,
}

impl AuditEvent {
    pub fn new(action: &'static str, subject_table: &'static str) -> Self {
        Self {
            actor_user_id: None,
            actor_kind: ActorKind::User,
            action,
            subject_table,
            subject_id: None,
            data: serde_json::json!({}),
            request_id: None,
            ip_hash: None,
        }
    }

    pub fn actor(mut self, kind: ActorKind, user_id: Option<Uuid>) -> Self {
        self.actor_kind = kind;
        self.actor_user_id = user_id;
        self
    }

    pub fn subject(mut self, id: Uuid) -> Self {
        self.subject_id = Some(id);
        self
    }

    pub fn data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }

    pub fn request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn ip_hash(mut self, ip_hash: Option<Vec<u8>>) -> Self {
        self.ip_hash = ip_hash;
        self
    }
}

/// Write one event.
///
/// Takes a connection rather than a pool so the caller can enlist it in the
/// transaction that performed the action: an audit row that commits while the
/// action rolls back is worse than no audit row.
pub async fn record(conn: &mut PgConnection, event: AuditEvent) -> Result<Uuid, AppError> {
    let id = new_id();

    sqlx::query(
        "INSERT INTO audit_log \
             (id, actor_user_id, actor_kind, action, subject_table, subject_id, \
              data, request_id, ip_hash) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(event.actor_user_id)
    .bind(event.actor_kind.as_str())
    .bind(event.action)
    .bind(event.subject_table)
    .bind(event.subject_id)
    .bind(&event.data)
    .bind(event.request_id.as_deref())
    .bind(event.ip_hash.as_deref())
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(id)
}

/// Read events for one subject, newest first. Used by tests and, later, by the
/// admin surface.
pub async fn for_subject(
    conn: &mut PgConnection,
    subject_table: &str,
    subject_id: Uuid,
    limit: i64,
) -> Result<Vec<(String, serde_json::Value)>, AppError> {
    sqlx::query_as(
        "SELECT action, data FROM audit_log \
          WHERE subject_table = $1 AND subject_id = $2 \
          ORDER BY created_at DESC, id DESC LIMIT $3",
    )
    .bind(subject_table)
    .bind(subject_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}
