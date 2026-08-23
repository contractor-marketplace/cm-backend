//! CSLB import runs and the licence register they populate.

use chrono::{DateTime, NaiveDate, Utc};
use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

/// Where a set of rows came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CslbMasterList,
    CslbCountyList,
}

impl Source {
    pub const ALL: [Self; 2] = [Self::CslbMasterList, Self::CslbCountyList];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CslbMasterList => "cslb_master_list",
            Self::CslbCountyList => "cslb_county_list",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|source| source.as_str() == value)
    }
}

/// Licence status, normalised. `status_raw` keeps CSLB's own string so a
/// mapping mistake is repairable without re-downloading the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseStatus {
    Active,
    Expired,
    Suspended,
    Inactive,
    Unknown,
}

impl LicenseStatus {
    pub const ALL: [Self; 5] = [
        Self::Active,
        Self::Expired,
        Self::Suspended,
        Self::Inactive,
        Self::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Suspended => "suspended",
            Self::Inactive => "inactive",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

impl RunStatus {
    pub const ALL: [Self; 3] = [Self::Running, Self::Succeeded, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Counters accumulated over one import.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunCounts {
    pub read: i32,
    pub inserted: i32,
    pub updated: i32,
    pub unchanged: i32,
    pub skipped: i32,
    pub rejected: i32,
}

/// One normalised licence row, ready to be stored.
#[derive(Debug, Clone)]
pub struct LicenseRecord {
    pub license_no: String,
    pub business_name: String,
    pub business_type: Option<String>,
    pub status: LicenseStatus,
    pub status_raw: String,
    pub issue_date: Option<NaiveDate>,
    pub expiration_date: Option<NaiveDate>,
    pub classifications: Vec<String>,
    pub bond_amount_cents: Option<i64>,
    pub workers_comp_status: Option<String>,
    pub address_line1: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub county: Option<String>,
    pub phone: Option<String>,
    pub raw: serde_json::Value,
    pub content_hash: Vec<u8>,
}

/// What happened to one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct StoredLicense {
    pub id: Uuid,
    pub outcome: UpsertOutcome,
}

/// Open a run. Refuses a file whose bytes have already been imported.
pub async fn begin_run(
    conn: &mut PgConnection,
    source: Source,
    file_name: &str,
    file_sha256: &[u8],
    snapshot_date: Option<NaiveDate>,
) -> Result<Uuid, AppError> {
    let already: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM license_import_runs \
          WHERE source = $1 AND source_file_sha256 = $2 AND status = 'succeeded'",
    )
    .bind(source.as_str())
    .bind(file_sha256)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    if let Some(id) = already {
        return Err(AppError::conflict(format!(
            "these exact bytes were already imported successfully by run {id}"
        )));
    }

    let id = new_id();
    sqlx::query(
        "INSERT INTO license_import_runs \
             (id, source, source_file_name, source_file_sha256, snapshot_date) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(source.as_str())
    .bind(file_name)
    .bind(file_sha256)
    .bind(snapshot_date)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(id)
}

pub async fn finish_run(
    conn: &mut PgConnection,
    run_id: Uuid,
    status: RunStatus,
    counts: RunCounts,
    error_text: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE license_import_runs \
            SET status = $2, rows_read = $3, rows_inserted = $4, rows_updated = $5, \
                rows_unchanged = $6, rows_skipped = $7, rows_rejected = $8, \
                error_text = $9, finished_at = now(), updated_at = now() \
          WHERE id = $1",
    )
    .bind(run_id)
    .bind(status.as_str())
    .bind(counts.read)
    .bind(counts.inserted)
    .bind(counts.updated)
    .bind(counts.unchanged)
    .bind(counts.skipped)
    .bind(counts.rejected)
    .bind(error_text)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Insert or update one licence, and append a version row when the content
/// actually changed.
///
/// Unchanged rows touch only `last_seen_at`/`last_run_id`, which is what makes
/// re-importing the same file a no-op in every way a caller can observe.
pub async fn upsert(
    conn: &mut PgConnection,
    run_id: Uuid,
    record: &LicenseRecord,
) -> Result<StoredLicense, AppError> {
    let existing: Option<(Uuid, Vec<u8>)> =
        sqlx::query_as("SELECT id, content_hash FROM license_records WHERE license_no = $1")
            .bind(&record.license_no)
            .fetch_optional(&mut *conn)
            .await
            .map_err(AppError::internal)?;

    let (id, outcome) = match existing {
        Some((id, hash)) if hash == record.content_hash => {
            sqlx::query(
                "UPDATE license_records SET last_seen_at = now(), last_run_id = $2 WHERE id = $1",
            )
            .bind(id)
            .bind(run_id)
            .execute(&mut *conn)
            .await
            .map_err(AppError::internal)?;

            return Ok(StoredLicense {
                id,
                outcome: UpsertOutcome::Unchanged,
            });
        }
        Some((id, _)) => {
            update(conn, id, run_id, record).await?;
            (id, UpsertOutcome::Updated)
        }
        None => {
            let id = insert(conn, run_id, record).await?;
            (id, UpsertOutcome::Inserted)
        }
    };

    // Append-only history, so "preserve the raw source" survives import #2.
    sqlx::query(
        "INSERT INTO license_record_versions (id, license_record_id, run_id, content_hash, raw) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (license_record_id, content_hash) DO NOTHING",
    )
    .bind(new_id())
    .bind(id)
    .bind(run_id)
    .bind(&record.content_hash)
    .bind(&record.raw)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(StoredLicense { id, outcome })
}

async fn insert(
    conn: &mut PgConnection,
    run_id: Uuid,
    record: &LicenseRecord,
) -> Result<Uuid, AppError> {
    let id = new_id();
    sqlx::query(
        "INSERT INTO license_records ( \
             id, license_no, business_name, business_type, status, status_raw, \
             issue_date, expiration_date, classifications, bond_amount_cents, \
             workers_comp_status, address_line1, city, state, postal_code, county, \
             phone, raw, content_hash, first_run_id, last_run_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$20)",
    )
    .bind(id)
    .bind(&record.license_no)
    .bind(&record.business_name)
    .bind(&record.business_type)
    .bind(record.status.as_str())
    .bind(&record.status_raw)
    .bind(record.issue_date)
    .bind(record.expiration_date)
    .bind(&record.classifications)
    .bind(record.bond_amount_cents)
    .bind(&record.workers_comp_status)
    .bind(&record.address_line1)
    .bind(&record.city)
    .bind(&record.state)
    .bind(&record.postal_code)
    .bind(&record.county)
    .bind(&record.phone)
    .bind(&record.raw)
    .bind(&record.content_hash)
    .bind(run_id)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(id)
}

async fn update(
    conn: &mut PgConnection,
    id: Uuid,
    run_id: Uuid,
    record: &LicenseRecord,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE license_records SET \
             business_name = $2, business_type = $3, status = $4, status_raw = $5, \
             issue_date = $6, expiration_date = $7, classifications = $8, \
             bond_amount_cents = $9, workers_comp_status = $10, address_line1 = $11, \
             city = $12, state = $13, postal_code = $14, county = $15, phone = $16, \
             raw = $17, content_hash = $18, last_run_id = $19, last_seen_at = now(), \
             updated_at = now() \
          WHERE id = $1",
    )
    .bind(id)
    .bind(&record.business_name)
    .bind(&record.business_type)
    .bind(record.status.as_str())
    .bind(&record.status_raw)
    .bind(record.issue_date)
    .bind(record.expiration_date)
    .bind(&record.classifications)
    .bind(record.bond_amount_cents)
    .bind(&record.workers_comp_status)
    .bind(&record.address_line1)
    .bind(&record.city)
    .bind(&record.state)
    .bind(&record.postal_code)
    .bind(&record.county)
    .bind(&record.phone)
    .bind(&record.raw)
    .bind(&record.content_hash)
    .bind(run_id)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Licence facts the verification service reads. Deliberately narrow: the badge
/// depends on status and expiry, and on nothing a claimant can write.
#[derive(Debug, Clone)]
pub struct LicenseFacts {
    pub license_no: String,
    pub status: LicenseStatus,
    pub expiration_date: Option<NaiveDate>,
    pub last_seen_at: DateTime<Utc>,
}

pub async fn facts_for_contractor(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Option<LicenseFacts>, AppError> {
    let row: Option<(String, String, Option<NaiveDate>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT l.license_no, l.status, l.expiration_date, l.last_seen_at \
           FROM license_records l \
           JOIN contractors c ON c.license_record_id = l.id \
          WHERE c.id = $1",
    )
    .bind(contractor_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    row.map(|(license_no, status, expiration_date, last_seen_at)| {
        Ok(LicenseFacts {
            license_no,
            status: LicenseStatus::parse(&status).ok_or_else(|| {
                AppError::internal(format!("unknown licence status in database: {status}"))
            })?,
            expiration_date,
            last_seen_at,
        })
    })
    .transpose()
}

/// The most recent successful import, for the "as of" date the API reports.
pub async fn latest_successful_snapshot(
    conn: &mut PgConnection,
) -> Result<Option<(Uuid, Option<NaiveDate>, DateTime<Utc>)>, AppError> {
    sqlx::query_as(
        "SELECT id, snapshot_date, finished_at FROM license_import_runs \
          WHERE status = 'succeeded' ORDER BY finished_at DESC LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}
