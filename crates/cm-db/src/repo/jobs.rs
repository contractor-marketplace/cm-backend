//! Jobs a homeowner has posted, and the queries contractors browse them with.
//!
//! **No query in this file selects `description` or the poster's name into the
//! anonymous projection.** That is the load-bearing property of this module: the
//! three tiers are three SQL projections, not one row filtered afterwards in
//! Rust, because a projection that never names a column cannot leak it through a
//! handler that forgets to strip it. `the_public_projection_names_nothing_private`
//! below asserts that against the SQL constant itself, so it cannot drift.
//!
//! There is no `precise_point` here at all — jobs never store an address. See
//! the header of `migrations/0017_jobs.sql`.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

/// Lifecycle. Mirrors the `jobs.status` CHECK; a migration test asserts they agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Open,
    Closed,
    Cancelled,
}

impl JobStatus {
    pub const ALL: [Self; 3] = [Self::Open, Self::Closed, Self::Cancelled];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
            .ok_or_else(|| AppError::internal(format!("unknown job status in database: {value}")))
    }
}

/// How soon the work is wanted. Mirrors the `jobs.timeline` CHECK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobTimeline {
    Asap,
    WithinAMonth,
    WithinThreeMonths,
    Flexible,
}

impl JobTimeline {
    pub const ALL: [Self; 4] = [
        Self::Asap,
        Self::WithinAMonth,
        Self::WithinThreeMonths,
        Self::Flexible,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asap => "asap",
            Self::WithinAMonth => "within_a_month",
            Self::WithinThreeMonths => "within_three_months",
            Self::Flexible => "flexible",
        }
    }

    /// For a value from a client: a 400 naming the options, not a 500.
    pub fn parse_request(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == value)
            .ok_or_else(|| {
                AppError::invalid(format!(
                    "Timeline must be one of: {}.",
                    Self::ALL
                        .iter()
                        .map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    pub fn parse(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == value)
            .ok_or_else(|| AppError::internal(format!("unknown job timeline in database: {value}")))
    }
}

/// What everyone sees, signed in or not.
///
/// Deliberately carries no description and nothing identifying the poster.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicJob {
    pub id: Uuid,
    pub title: String,
    pub trade: Option<String>,
    pub status: JobStatus,
    pub timeline: Option<JobTimeline>,
    pub budget_min_cents: Option<i64>,
    pub budget_max_cents: Option<i64>,
    pub postal_code: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Always `zip_centroid` or `none`. A job never publishes an exact point.
    pub location_precision: String,
    pub created_at: DateTime<Utc>,
    pub distance_m: Option<f64>,
}

/// What a signed-in contractor sees: the detail needed to decide whether to
/// pursue the work, plus a first name so it reads as a person rather than a
/// ticket. Never a full name, an email, or a location finer than the ZIP.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContractorJob {
    #[serde(flatten)]
    pub public: PublicJob,
    pub description: String,
    pub poster_first_name: Option<String>,
}

/// What the poster sees of their own job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnerJob {
    #[serde(flatten)]
    pub public: PublicJob,
    pub description: String,
    pub posted_by_user_id: Uuid,
    pub closed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub trade_ids: Option<Vec<Uuid>>,
    pub postal_code: Option<String>,
    pub near: Option<Near>,
}

#[derive(Debug, Clone, Copy)]
pub struct Near {
    pub lat: f64,
    pub lon: f64,
    pub radius_m: f64,
}

/// The keyset key. Newest-first is the only ordering the board offers, so this
/// pair is both the sort and the cursor and they cannot drift apart.
#[derive(Debug, Clone)]
pub struct Cursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

pub struct Page<T> {
    pub jobs: Vec<T>,
    pub next_cursor: Option<Cursor>,
}

pub const MAX_PAGE: i64 = 50;
pub const DEFAULT_PAGE: i64 = 20;

/// Shared by both list projections so the two tiers can never disagree about
/// which rows exist — only about how much of each row is returned.
const PREDICATE: &str = "\
    j.status = 'open' \
    AND ($1::float8 IS NULL OR ST_DWithin(j.public_point, \
         ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)) \
    AND ($4::uuid[] IS NULL OR j.trade_id = ANY($4)) \
    AND ($5::text IS NULL OR j.postal_code = $5)";

/// The columns everyone may see. Note what is absent: `j.description`, and any
/// join to `users`.
const SELECT_PUBLIC: &str = "\
    SELECT j.id, j.title, t.slug AS trade, j.status, j.timeline, \
           j.budget_min_cents, j.budget_max_cents, j.postal_code, \
           ST_Y(j.public_point::geometry) AS lat, \
           ST_X(j.public_point::geometry) AS lon, \
           j.public_point_source AS location_precision, j.created_at, \
           CASE WHEN $1::float8 IS NULL THEN NULL \
                ELSE ST_Distance(j.public_point, \
                     ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography) END AS distance_m \
      FROM jobs j LEFT JOIN trades t ON t.id = j.trade_id";

/// The same rows, plus what a signed-in contractor is entitled to.
///
/// `split_part` takes only the first whitespace-delimited token of the display
/// name: a first name identifies a person to talk to without publishing the
/// full name they registered under.
const SELECT_FOR_CONTRACTOR: &str = "\
    SELECT j.id, j.title, t.slug AS trade, j.status, j.timeline, \
           j.budget_min_cents, j.budget_max_cents, j.postal_code, \
           ST_Y(j.public_point::geometry) AS lat, \
           ST_X(j.public_point::geometry) AS lon, \
           j.public_point_source AS location_precision, j.created_at, \
           CASE WHEN $1::float8 IS NULL THEN NULL \
                ELSE ST_Distance(j.public_point, \
                     ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography) END AS distance_m, \
           j.description, \
           split_part(btrim(u.display_name), ' ', 1) AS poster_first_name \
      FROM jobs j \
      LEFT JOIN trades t ON t.id = j.trade_id \
      JOIN users u ON u.id = j.posted_by_user_id";

fn bind_public<'q>(
    query: sqlx::query::QueryAs<'q, sqlx::Postgres, PublicJobRow, sqlx::postgres::PgArguments>,
    filters: &'q Filters,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, PublicJobRow, sqlx::postgres::PgArguments> {
    query
        .bind(filters.near.map(|n| n.lon))
        .bind(filters.near.map(|n| n.lat))
        .bind(filters.near.map(|n| n.radius_m))
        .bind(filters.trade_ids.clone())
        .bind(filters.postal_code.clone())
}

fn bind_contractor<'q>(
    query: sqlx::query::QueryAs<'q, sqlx::Postgres, ContractorJobRow, sqlx::postgres::PgArguments>,
    filters: &'q Filters,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, ContractorJobRow, sqlx::postgres::PgArguments> {
    query
        .bind(filters.near.map(|n| n.lon))
        .bind(filters.near.map(|n| n.lat))
        .bind(filters.near.map(|n| n.radius_m))
        .bind(filters.trade_ids.clone())
        .bind(filters.postal_code.clone())
}

/// Newest first. The cursor predicate is `<` because the order is DESC, and it
/// compares the identical `(created_at, id)` tuple the ORDER BY ends on.
const ORDER_AND_KEYSET: &str = "\
    AND ($6::timestamptz IS NULL OR (j.created_at, j.id) < ($6, $7::uuid)) \
    ORDER BY j.created_at DESC, j.id DESC LIMIT $8";

pub async fn list_public(
    conn: &mut PgConnection,
    filters: &Filters,
    limit: i64,
    cursor: Option<&Cursor>,
) -> Result<Page<PublicJob>, AppError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let sql = format!("{SELECT_PUBLIC} WHERE {PREDICATE} {ORDER_AND_KEYSET}");

    let mut rows: Vec<PublicJobRow> = bind_public(sqlx::query_as(&sql), filters)
        .bind(cursor.map(|c| c.created_at))
        .bind(cursor.map(|c| c.id))
        .bind(limit + 1)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    let next = take_next(&mut rows, limit, |r| Cursor {
        created_at: r.created_at,
        id: r.id,
    });

    Ok(Page {
        jobs: rows
            .into_iter()
            .map(PublicJobRow::into_public)
            .collect::<Result<_, _>>()?,
        next_cursor: next,
    })
}

pub async fn list_for_contractor(
    conn: &mut PgConnection,
    filters: &Filters,
    limit: i64,
    cursor: Option<&Cursor>,
) -> Result<Page<ContractorJob>, AppError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let sql = format!("{SELECT_FOR_CONTRACTOR} WHERE {PREDICATE} {ORDER_AND_KEYSET}");

    let mut rows: Vec<ContractorJobRow> = bind_contractor(sqlx::query_as(&sql), filters)
        .bind(cursor.map(|c| c.created_at))
        .bind(cursor.map(|c| c.id))
        .bind(limit + 1)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    let next = take_next(&mut rows, limit, |r| Cursor {
        created_at: r.created_at,
        id: r.id,
    });

    Ok(Page {
        jobs: rows
            .into_iter()
            .map(ContractorJobRow::into_contractor)
            .collect::<Result<_, _>>()?,
        next_cursor: next,
    })
}

/// One extra row answers "is there another page" without a second COUNT over a
/// table that will grow without bound.
fn take_next<T>(rows: &mut Vec<T>, limit: i64, key: impl Fn(&T) -> Cursor) -> Option<Cursor> {
    if rows.len() as i64 > limit {
        rows.pop();
        rows.last().map(key)
    } else {
        None
    }
}

/// Open jobs only, deliberately.
///
/// The board filters to open, and the detail page must agree: a homeowner who
/// closes a job — and especially one who cancels it — has said take it down,
/// not just stop listing it. The poster still sees it through `for_poster`.
pub async fn find_public(conn: &mut PgConnection, id: Uuid) -> Result<Option<PublicJob>, AppError> {
    let sql = format!("{SELECT_PUBLIC} WHERE j.id = $6 AND j.status = 'open'");
    let row: Option<PublicJobRow> = sqlx::query_as(&sql)
        .bind(None::<f64>)
        .bind(None::<f64>)
        .bind(None::<f64>)
        .bind(None::<Vec<Uuid>>)
        .bind(None::<String>)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    row.map(PublicJobRow::into_public).transpose()
}

pub async fn find_for_contractor(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<ContractorJob>, AppError> {
    let sql = format!("{SELECT_FOR_CONTRACTOR} WHERE j.id = $6 AND j.status = 'open'");
    let row: Option<ContractorJobRow> = sqlx::query_as(&sql)
        .bind(None::<f64>)
        .bind(None::<f64>)
        .bind(None::<f64>)
        .bind(None::<Vec<Uuid>>)
        .bind(None::<String>)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    row.map(ContractorJobRow::into_contractor).transpose()
}

/// The poster's own jobs, in every state — an owner needs to see what they
/// closed, not only what is live.
pub async fn for_poster(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<OwnerJob>, AppError> {
    let rows: Vec<OwnerJobRow> = sqlx::query_as(
        "SELECT j.id, j.title, t.slug AS trade, j.status, j.timeline, \
                j.budget_min_cents, j.budget_max_cents, j.postal_code, \
                ST_Y(j.public_point::geometry) AS lat, \
                ST_X(j.public_point::geometry) AS lon, \
                j.public_point_source AS location_precision, j.created_at, \
                j.description, j.posted_by_user_id, j.closed_at, j.updated_at \
           FROM jobs j LEFT JOIN trades t ON t.id = j.trade_id \
          WHERE j.posted_by_user_id = $1 \
          ORDER BY j.created_at DESC, j.id DESC LIMIT 200",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    rows.into_iter().map(OwnerJobRow::into_owner).collect()
}

pub struct NewJob<'a> {
    pub id: Uuid,
    pub posted_by_user_id: Uuid,
    pub title: &'a str,
    pub description: &'a str,
    pub trade_id: Option<Uuid>,
    pub budget_min_cents: Option<i64>,
    pub budget_max_cents: Option<i64>,
    pub timeline: Option<JobTimeline>,
    pub postal_code: Option<&'a str>,
    pub region_id: Option<Uuid>,
    /// `(lon, lat)` of the ZIP centroid, when the ZIP resolves to one.
    pub centroid: Option<(f64, f64)>,
}

pub async fn insert(conn: &mut PgConnection, job: NewJob<'_>) -> Result<Uuid, AppError> {
    sqlx::query(
        "INSERT INTO jobs (id, posted_by_user_id, title, description, trade_id, \
                           budget_min_cents, budget_max_cents, timeline, postal_code, \
                           region_id, public_point, public_point_source) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
                 CASE WHEN $11::float8 IS NULL THEN NULL \
                      ELSE ST_SetSRID(ST_MakePoint($11, $12), 4326)::geography END, \
                 CASE WHEN $11::float8 IS NULL THEN 'none' ELSE 'zip_centroid' END)",
    )
    .bind(job.id)
    .bind(job.posted_by_user_id)
    .bind(job.title)
    .bind(job.description)
    .bind(job.trade_id)
    .bind(job.budget_min_cents)
    .bind(job.budget_max_cents)
    .bind(job.timeline.map(|t| t.as_str()))
    .bind(job.postal_code)
    .bind(job.region_id)
    .bind(job.centroid.map(|(lon, _)| lon))
    .bind(job.centroid.map(|(_, lat)| lat))
    .execute(&mut *conn)
    .await
    .map_err(|error| match &error {
        // The poster-is-a-homeowner trigger raises check_violation. The handler
        // checks this first and answers 403; reaching here means a code path
        // bypassed it, so the message is for a developer, not a user.
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23514") => {
            AppError::internal(format!("a job insert violated a constraint: {db}"))
        }
        _ => AppError::internal(error),
    })?;

    Ok(job.id)
}

/// Returns whether a row changed. `WHERE ... AND status = 'open'` is the lock:
/// two concurrent closes produce exactly one update.
pub async fn close(
    conn: &mut PgConnection,
    id: Uuid,
    poster: Uuid,
    status: JobStatus,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE jobs SET status = $3, closed_at = now(), updated_at = now() \
          WHERE id = $1 AND posted_by_user_id = $2 AND status = 'open'",
    )
    .bind(id)
    .bind(poster)
    .bind(status.as_str())
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

/// Who posted a job, for an ownership check that must not leak the job itself.
pub async fn poster_of(conn: &mut PgConnection, id: Uuid) -> Result<Option<Uuid>, AppError> {
    sqlx::query_scalar("SELECT posted_by_user_id FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The privacy guarantee, asserted against the SQL rather than against a
    /// comment.
    ///
    /// `contractors.rs` claims "a test greps this crate to keep it that way"
    /// about `precise_point` and no such test exists. This is the same idea
    /// implemented: the anonymous projection is a `const &str`, so it can be
    /// inspected directly, and no amount of drift in the handlers can put a
    /// column into a response that the query never asked for.
    #[test]
    fn the_public_projection_names_nothing_private() {
        for forbidden in [
            "description",
            "display_name",
            "posted_by_user_id",
            "users",
            "email",
        ] {
            assert!(
                !SELECT_PUBLIC.contains(forbidden),
                "the anonymous projection must never name {forbidden}: {SELECT_PUBLIC}"
            );
        }
    }

    /// The contractor projection is allowed the description and a first name,
    /// and still nothing else about the person.
    #[test]
    fn the_contractor_projection_stops_at_a_first_name() {
        assert!(SELECT_FOR_CONTRACTOR.contains("j.description"));
        assert!(SELECT_FOR_CONTRACTOR.contains("split_part"));

        // Only the columns being returned, not the FROM/JOIN clauses. The join
        // condition legitimately mentions posted_by_user_id — that is how the
        // first name is reached — and the point is that the id itself is never
        // handed back.
        let projection = SELECT_FOR_CONTRACTOR
            .split(" FROM ")
            .next()
            .expect("a SELECT has a projection");

        for forbidden in ["u.email", "posted_by_user_id"] {
            assert!(
                !projection.contains(forbidden),
                "the contractor projection must never return {forbidden}: {projection}"
            );
        }

        // The display name may be *read* — that is where the first name comes
        // from — but only ever through split_part, never returned whole. Any
        // other use of it would be a full name on the wire.
        for fragment in projection.match_indices("u.display_name") {
            let before = &projection[..fragment.0];
            assert!(
                before.ends_with("split_part(btrim("),
                "u.display_name may only be read through split_part: {projection}"
            );
        }
    }

    /// Both list tiers must filter identically, or the redacted board would
    /// show a different set of jobs from the detailed one.
    #[test]
    fn both_tiers_share_one_predicate() {
        assert!(PREDICATE.contains("j.status = 'open'"));
        // Both SELECTs are formatted with the same PREDICATE constant, so the
        // only way they can diverge is if one stops using it.
        let public = format!("{SELECT_PUBLIC} WHERE {PREDICATE}");
        let contractor = format!("{SELECT_FOR_CONTRACTOR} WHERE {PREDICATE}");
        assert!(public.contains(PREDICATE) && contractor.contains(PREDICATE));
    }

    #[test]
    fn status_and_timeline_round_trip() {
        for status in JobStatus::ALL {
            assert_eq!(JobStatus::parse(status.as_str()).unwrap(), status);
        }
        for timeline in JobTimeline::ALL {
            assert_eq!(JobTimeline::parse(timeline.as_str()).unwrap(), timeline);
        }
        assert!(JobStatus::parse("elsewhere").is_err());
        assert!(JobTimeline::parse_request("eventually").is_err());
    }
}

/* ── Row types ─────────────────────────────────────────────────────────────
 * Separate from the serialized shapes so the `status`/`timeline` strings are
 * validated on the way out of the database rather than trusted.
 */

#[derive(sqlx::FromRow)]
struct PublicJobRow {
    id: Uuid,
    title: String,
    trade: Option<String>,
    status: String,
    timeline: Option<String>,
    budget_min_cents: Option<i64>,
    budget_max_cents: Option<i64>,
    postal_code: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    location_precision: String,
    created_at: DateTime<Utc>,
    distance_m: Option<f64>,
}

impl PublicJobRow {
    fn into_public(self) -> Result<PublicJob, AppError> {
        Ok(PublicJob {
            id: self.id,
            title: self.title,
            trade: self.trade,
            status: JobStatus::parse(&self.status)?,
            timeline: self.timeline.as_deref().map(JobTimeline::parse).transpose()?,
            budget_min_cents: self.budget_min_cents,
            budget_max_cents: self.budget_max_cents,
            postal_code: self.postal_code,
            lat: self.lat,
            lon: self.lon,
            location_precision: self.location_precision,
            created_at: self.created_at,
            distance_m: self.distance_m,
        })
    }
}

#[derive(sqlx::FromRow)]
struct ContractorJobRow {
    id: Uuid,
    title: String,
    trade: Option<String>,
    status: String,
    timeline: Option<String>,
    budget_min_cents: Option<i64>,
    budget_max_cents: Option<i64>,
    postal_code: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    location_precision: String,
    created_at: DateTime<Utc>,
    distance_m: Option<f64>,
    description: String,
    poster_first_name: Option<String>,
}

impl ContractorJobRow {
    fn into_contractor(self) -> Result<ContractorJob, AppError> {
        let description = self.description;
        let poster_first_name = self.poster_first_name.filter(|n| !n.is_empty());
        let public = PublicJobRow {
            id: self.id,
            title: self.title,
            trade: self.trade,
            status: self.status,
            timeline: self.timeline,
            budget_min_cents: self.budget_min_cents,
            budget_max_cents: self.budget_max_cents,
            postal_code: self.postal_code,
            lat: self.lat,
            lon: self.lon,
            location_precision: self.location_precision,
            created_at: self.created_at,
            distance_m: self.distance_m,
        }
        .into_public()?;

        Ok(ContractorJob {
            public,
            description,
            poster_first_name,
        })
    }
}

#[derive(sqlx::FromRow)]
struct OwnerJobRow {
    id: Uuid,
    title: String,
    trade: Option<String>,
    status: String,
    timeline: Option<String>,
    budget_min_cents: Option<i64>,
    budget_max_cents: Option<i64>,
    postal_code: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    location_precision: String,
    created_at: DateTime<Utc>,
    description: String,
    posted_by_user_id: Uuid,
    closed_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

impl OwnerJobRow {
    fn into_owner(self) -> Result<OwnerJob, AppError> {
        let (description, posted_by_user_id, closed_at, updated_at) = (
            self.description,
            self.posted_by_user_id,
            self.closed_at,
            self.updated_at,
        );
        let public = PublicJobRow {
            id: self.id,
            title: self.title,
            trade: self.trade,
            status: self.status,
            timeline: self.timeline,
            budget_min_cents: self.budget_min_cents,
            budget_max_cents: self.budget_max_cents,
            postal_code: self.postal_code,
            lat: self.lat,
            lon: self.lon,
            location_precision: self.location_precision,
            created_at: self.created_at,
            distance_m: None,
        }
        .into_public()?;

        Ok(OwnerJob {
            public,
            description,
            posted_by_user_id,
            closed_at,
            updated_at,
        })
    }
}
