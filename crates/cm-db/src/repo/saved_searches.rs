//! Saved searches, and the reverse match that turns new jobs into digests.
//!
//! A saved search is the live board's `Filters`, frozen as typed columns. The
//! reverse match in `matches_for_jobs` is the board's `PREDICATE` with the
//! sides swapped — each clause here mirrors one there, kept adjacent in the
//! comments, and the drift guard test in `cm-api/tests/saved_searches.rs`
//! asserts the two agree on real rows.

use chrono::{DateTime, Utc};
use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

use super::jobs::{BuildType, Filters, JobTimeline};

/// The most searches one account may hold. A person curates a handful; only a
/// scraper wants hundreds.
pub const MAX_PER_USER: i64 = 20;

#[derive(Debug, Clone)]
pub struct SavedSearch {
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub query: Option<String>,
    pub trade_ids: Option<Vec<Uuid>>,
    pub postal_code: Option<String>,
    pub center: Option<(f64, f64)>,
    pub radius_m: Option<f64>,
    pub timeline: Option<String>,
    pub build_type: Option<String>,
    pub budget_min_cents: Option<i64>,
    pub notify: bool,
    pub last_notified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

const SELECT_SEARCH: &str = "\
    SELECT id, user_id, name, query, trade_ids, postal_code, \
           ST_Y(center::geometry) AS lat, ST_X(center::geometry) AS lon, radius_m, \
           timeline, build_type, budget_min_cents, notify, last_notified_at, created_at \
      FROM saved_searches";

/// Save one search. The per-user cap is enforced inside the INSERT, so two
/// racing saves cannot both sneak past it by reading the same count.
pub async fn create(
    conn: &mut PgConnection,
    user_id: Uuid,
    name: &str,
    filters: &Filters,
) -> Result<SavedSearch, AppError> {
    let (lat, lon, radius_m) = match filters.near {
        Some(near) => (Some(near.lat), Some(near.lon), Some(near.radius_m)),
        None => (None, None, None),
    };
    let query_trade_ids =
        (!filters.query_trade_ids.is_empty()).then_some(filters.query_trade_ids.as_slice());

    let row: Option<SearchRow> = sqlx::query_as(
        "INSERT INTO saved_searches \
             (id, user_id, name, query, query_trade_ids, trade_ids, postal_code, \
              center, radius_m, timeline, build_type, budget_min_cents) \
         SELECT $1, $2, $3, $4, $5, $6, $7, \
                CASE WHEN $8::float8 IS NULL THEN NULL \
                     ELSE ST_SetSRID(ST_MakePoint($9, $8), 4326)::geography END, \
                $10, $11, $12, $13 \
          WHERE (SELECT count(*) FROM saved_searches WHERE user_id = $2) < $14 \
      RETURNING id, user_id, name, query, trade_ids, postal_code, \
                ST_Y(center::geometry) AS lat, ST_X(center::geometry) AS lon, radius_m, \
                timeline, build_type, budget_min_cents, notify, last_notified_at, created_at",
    )
    .bind(new_id())
    .bind(user_id)
    .bind(name)
    .bind(&filters.query)
    .bind(query_trade_ids)
    .bind(&filters.trade_ids)
    .bind(&filters.postal_code)
    .bind(lat)
    .bind(lon)
    .bind(radius_m)
    .bind(filters.timeline.map(JobTimeline::as_str))
    .bind(filters.build_type.map(BuildType::as_str))
    .bind(filters.budget_min_cents)
    .bind(MAX_PER_USER)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    row.map(Into::into).ok_or_else(|| {
        AppError::invalid(format!(
            "You already have {MAX_PER_USER} saved searches. Delete one to save another."
        ))
    })
}

pub async fn list_for_user(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<Vec<SavedSearch>, AppError> {
    let rows: Vec<SearchRow> = sqlx::query_as(&format!(
        "{SELECT_SEARCH} WHERE user_id = $1 ORDER BY created_at DESC"
    ))
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Owner-scoped delete. Whether a row went away is the caller's 404 signal.
pub async fn delete(conn: &mut PgConnection, user_id: Uuid, id: Uuid) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM saved_searches WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

/// Stop a search's email without deleting the search. Idempotent, and quiet
/// about whether the row existed — RFC 8058 one-click posts may repeat.
pub async fn set_notify_off(conn: &mut PgConnection, id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE saved_searches SET notify = false, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    Ok(())
}

/// Claim every job not yet matched against saved searches, oldest first.
///
/// `FOR UPDATE SKIP LOCKED` for the same reason the queues use it: two alert
/// passes running at once split the work instead of double-mailing it.
pub async fn claim_unmatched_jobs(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query_scalar(
        "SELECT id FROM jobs WHERE alerts_matched_at IS NULL \
          ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// One (search, job) pair that should alert.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AlertMatch {
    pub search_id: Uuid,
    pub search_name: String,
    pub user_id: Uuid,
    pub email: String,
    pub job_id: Uuid,
}

/// The reverse match: which saved searches does each of these jobs satisfy?
///
/// Every clause mirrors one in the board `PREDICATE` (repo/jobs.rs), with the
/// job column on the left as it is there:
///   status='open'        — closed or cancelled jobs alert nobody
///   ST_DWithin           — PREDICATE $1..$3
///   trade_ids            — PREDICATE $4
///   postal_code          — PREDICATE $5
///   search_doc / aliases — PREDICATE $6..$7
///   timeline             — PREDICATE $8
///   build_type           — PREDICATE $9
///   budget_max_cents     — PREDICATE $10
// ponytail: sequential scan over saved_searches per batch; add array/GiST
// indexes when searches reach tens of thousands.
pub async fn matches_for_jobs(
    conn: &mut PgConnection,
    job_ids: &[Uuid],
) -> Result<Vec<AlertMatch>, AppError> {
    sqlx::query_as(
        "SELECT s.id AS search_id, s.name AS search_name, s.user_id, u.email, j.id AS job_id \
           FROM jobs j \
           JOIN saved_searches s ON s.notify \
            AND (s.trade_ids IS NULL OR j.trade_id = ANY (s.trade_ids)) \
            AND (s.postal_code IS NULL OR j.postal_code = s.postal_code) \
            AND (s.center IS NULL OR ST_DWithin(j.public_point, s.center, s.radius_m)) \
            AND (s.query IS NULL \
                 OR j.search_doc @@ websearch_to_tsquery('public.english_unaccent', s.query) \
                 OR (s.query_trade_ids IS NOT NULL AND j.trade_id = ANY (s.query_trade_ids))) \
            AND (s.timeline IS NULL OR j.timeline = s.timeline) \
            AND (s.build_type IS NULL OR j.build_type = s.build_type) \
            AND (s.budget_min_cents IS NULL OR j.budget_max_cents >= s.budget_min_cents) \
           JOIN users u ON u.id = s.user_id AND u.status = 'active' \
          WHERE j.id = ANY ($1) AND j.status = 'open' \
            AND j.posted_by_user_id <> s.user_id \
          ORDER BY s.user_id, j.created_at",
    )
    .bind(job_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// What a digest line needs to say about a job.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AlertJob {
    pub id: Uuid,
    pub title: String,
    pub trade: Option<String>,
    pub postal_code: String,
    pub timeline: String,
    pub budget_min_cents: Option<i64>,
    pub budget_max_cents: Option<i64>,
}

pub async fn alert_jobs(
    conn: &mut PgConnection,
    job_ids: &[Uuid],
) -> Result<Vec<AlertJob>, AppError> {
    sqlx::query_as(
        "SELECT j.id, j.title, t.name AS trade, j.postal_code, j.timeline, \
                j.budget_min_cents, j.budget_max_cents \
           FROM jobs j LEFT JOIN trades t ON t.id = j.trade_id \
          WHERE j.id = ANY ($1)",
    )
    .bind(job_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Every claimed job is marked, matched or not: a job that alerted nobody has
/// still been considered, and must not be reconsidered next week.
pub async fn mark_jobs_matched(conn: &mut PgConnection, job_ids: &[Uuid]) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE jobs SET alerts_matched_at = now(), updated_at = now() \
                  WHERE id = ANY ($1)",
    )
    .bind(job_ids)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

pub async fn touch_notified(conn: &mut PgConnection, search_ids: &[Uuid]) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE saved_searches SET last_notified_at = now(), updated_at = now() \
          WHERE id = ANY ($1)",
    )
    .bind(search_ids)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

#[derive(sqlx::FromRow)]
struct SearchRow {
    id: Uuid,
    user_id: Uuid,
    name: String,
    query: Option<String>,
    trade_ids: Option<Vec<Uuid>>,
    postal_code: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    radius_m: Option<f64>,
    timeline: Option<String>,
    build_type: Option<String>,
    budget_min_cents: Option<i64>,
    notify: bool,
    last_notified_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl From<SearchRow> for SavedSearch {
    fn from(row: SearchRow) -> Self {
        Self {
            id: row.id,
            user_id: row.user_id,
            name: row.name,
            query: row.query,
            trade_ids: row.trade_ids,
            postal_code: row.postal_code,
            center: match (row.lat, row.lon) {
                (Some(lat), Some(lon)) => Some((lat, lon)),
                _ => None,
            },
            radius_m: row.radius_m,
            timeline: row.timeline,
            build_type: row.build_type,
            budget_min_cents: row.budget_min_cents,
            notify: row.notify,
            last_notified_at: row.last_notified_at,
            created_at: row.created_at,
        }
    }
}
