//! Contractor claims and their evidence.

use chrono::{DateTime, Utc};
use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Pending,
    Approved,
    Rejected,
    Withdrawn,
}

impl ClaimStatus {
    pub const ALL: [Self; 4] = [
        Self::Pending,
        Self::Approved,
        Self::Rejected,
        Self::Withdrawn,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Withdrawn => "withdrawn",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimMethod {
    LicensePhoneOtp,
    LicenseMailCode,
    ManualReview,
}

impl ClaimMethod {
    pub const ALL: [Self; 3] = [
        Self::LicensePhoneOtp,
        Self::LicenseMailCode,
        Self::ManualReview,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LicensePhoneOtp => "license_phone_otp",
            Self::LicenseMailCode => "license_mail_code",
            Self::ManualReview => "manual_review",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == value)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Claim {
    pub id: Uuid,
    pub contractor_id: Uuid,
    pub user_id: Uuid,
    pub status: ClaimStatus,
    pub method: ClaimMethod,
    pub evidence: serde_json::Value,
    pub decision_note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Claim {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let status: String = row.try_get("status")?;
        let method: String = row.try_get("method")?;

        Ok(Self {
            id: row.try_get("id")?,
            contractor_id: row.try_get("contractor_id")?,
            user_id: row.try_get("user_id")?,
            status: ClaimStatus::parse(&status).ok_or_else(|| {
                sqlx::Error::Decode(format!("unknown claim status: {status}").into())
            })?,
            method: ClaimMethod::parse(&method).ok_or_else(|| {
                sqlx::Error::Decode(format!("unknown claim method: {method}").into())
            })?,
            evidence: row.try_get("evidence")?,
            decision_note: row.try_get("decision_note")?,
            created_at: row.try_get("created_at")?,
            decided_at: row.try_get("decided_at")?,
        })
    }
}

const SELECT: &str = "SELECT id, contractor_id, user_id, status, method, evidence, \
                             decision_note, created_at, decided_at FROM contractor_claims";

/// Open a claim.
///
/// The partial unique indexes do the real work: one pending claim per pair, one
/// approved claim per contractor, one per user. A duplicate is a conflict, not
/// a 500.
pub async fn open(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    user_id: Uuid,
    method: ClaimMethod,
    evidence: &serde_json::Value,
) -> Result<Claim, AppError> {
    sqlx::query_as(
        "INSERT INTO contractor_claims (id, contractor_id, user_id, method, evidence) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, contractor_id, user_id, status, method, evidence, decision_note, \
                   created_at, decided_at",
    )
    .bind(new_id())
    .bind(contractor_id)
    .bind(user_id)
    .bind(method.as_str())
    .bind(evidence)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db) => match db.constraint() {
            Some("contractor_claims_one_pending_per_pair") => {
                AppError::conflict("You already have a claim pending on this listing.")
            }
            Some("contractor_claims_one_approved_per_contractor") => {
                AppError::conflict("This listing has already been claimed.")
            }
            Some("contractor_claims_one_approved_per_user") => {
                AppError::conflict("Your account has already claimed a listing.")
            }
            _ => AppError::internal(error),
        },
        _ => AppError::internal(error),
    })
}

pub async fn find(conn: &mut PgConnection, id: Uuid) -> Result<Option<Claim>, AppError> {
    sqlx::query_as(&format!("{SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)
}

pub async fn for_user(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<Claim>, AppError> {
    sqlx::query_as(&format!(
        "{SELECT} WHERE user_id = $1 ORDER BY created_at DESC"
    ))
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn pending(conn: &mut PgConnection, limit: i64) -> Result<Vec<Claim>, AppError> {
    sqlx::query_as(&format!(
        "{SELECT} WHERE status = 'pending' ORDER BY created_at LIMIT $1"
    ))
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Move a claim out of `pending`, but only from `pending`.
///
/// The guard is what makes two simultaneous approvals safe: the second one
/// changes no rows and is told so, rather than both succeeding.
pub async fn decide(
    conn: &mut PgConnection,
    claim_id: Uuid,
    status: ClaimStatus,
    decided_by: Option<Uuid>,
    note: Option<&str>,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE contractor_claims \
            SET status = $2, decided_at = now(), decided_by = $3, decision_note = $4, \
                updated_at = now() \
          WHERE id = $1 AND status = 'pending'",
    )
    .bind(claim_id)
    .bind(status.as_str())
    .bind(decided_by)
    .bind(note)
    .execute(&mut *conn)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db)
            if db.constraint() == Some("contractor_claims_one_approved_per_contractor")
                || db.constraint() == Some("contractor_claims_one_approved_per_user") =>
        {
            AppError::conflict("That listing or account already has an approved claim.")
        }
        _ => AppError::internal(error),
    })?;

    Ok(result.rows_affected() == 1)
}

/// Record one piece of verification evidence.
#[allow(clippy::too_many_arguments)]
pub async fn record_check(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    claim_id: Option<Uuid>,
    kind: &str,
    outcome: &str,
    evidence: &serde_json::Value,
    performed_by: Option<Uuid>,
) -> Result<Uuid, AppError> {
    let id = new_id();
    sqlx::query(
        "INSERT INTO verification_checks \
             (id, contractor_id, claim_id, kind, outcome, evidence, performed_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(contractor_id)
    .bind(claim_id)
    .bind(kind)
    .bind(outcome)
    .bind(evidence)
    .bind(performed_by)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(id)
}

/// The evidence trail for one contractor, newest first.
pub async fn checks_for_contractor(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    limit: i64,
) -> Result<Vec<(String, String, serde_json::Value, DateTime<Utc>)>, AppError> {
    sqlx::query_as(
        "SELECT kind, outcome, evidence, observed_at FROM verification_checks \
          WHERE contractor_id = $1 ORDER BY observed_at DESC LIMIT $2",
    )
    .bind(contractor_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}
