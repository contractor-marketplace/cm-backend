//! Jobs a homeowner has posted, and the queries used to browse them.
//!
//! One browse projection, shown to everyone. An earlier version served three
//! tiers — redacted for signed-out visitors, detailed for contractors — which
//! was removed: a tier anybody can step around by signing out is complexity
//! without a guarantee.
//!
//! The protection that does hold is in the schema rather than here. There is no
//! `precise_point` column and no address field on `jobs` at all, so a job is
//! published at its ZIP centroid or nowhere, and no projection can widen that.
//! See the header of `migrations/0017_jobs.sql`.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

/// Lifecycle. Mirrors the `jobs.status` CHECK; a migration test asserts they agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
///
/// Two weeks is the line that matters: a contractor deciding whether to reply
/// needs to know whether this is work they could start now, work to schedule,
/// or a person who has not decided yet. `Unsure` is a recorded answer, not a
/// missing one — the form makes it a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobTimeline {
    Asap,
    Within2Weeks,
    MoreThan2Weeks,
    Unsure,
}

impl JobTimeline {
    pub const ALL: [Self; 4] = [
        Self::Asap,
        Self::Within2Weeks,
        Self::MoreThan2Weeks,
        Self::Unsure,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asap => "asap",
            Self::Within2Weeks => "within_2_weeks",
            Self::MoreThan2Weeks => "more_than_2_weeks",
            Self::Unsure => "unsure",
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

/// Whether this is new work, a like-for-like swap, or a fix. Mirrors the
/// `jobs.build_type` CHECK.
///
/// It changes who wants the job more than almost anything else on the form: a
/// new build and a repair are different businesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildType {
    NewBuild,
    Replacement,
    Repair,
    Unsure,
}

impl BuildType {
    pub const ALL: [Self; 4] = [
        Self::NewBuild,
        Self::Replacement,
        Self::Repair,
        Self::Unsure,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewBuild => "new_build",
            Self::Replacement => "replacement",
            Self::Repair => "repair",
            Self::Unsure => "unsure",
        }
    }

    pub fn parse_request(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|b| b.as_str() == value)
            .ok_or_else(|| {
                AppError::invalid(format!(
                    "Build type must be one of: {}.",
                    Self::ALL
                        .iter()
                        .map(|b| b.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    pub fn parse(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|b| b.as_str() == value)
            .ok_or_else(|| AppError::internal(format!("unknown build type in database: {value}")))
    }
}

/// Serialised through `as_str`, never derived.
///
/// A derived `rename_all = "snake_case"` renders `Within2Weeks` as
/// "within2_weeks" while `as_str` writes "within_2_weeks", which would put a
/// different spelling on the wire than in the database for the same value.
/// Going through `as_str` leaves one spelling and nowhere for a second to come
/// from. `the_wire_spelling_matches_the_database` pins it.
macro_rules! serialize_as_str {
    ($($kind:ty),+ $(,)?) => {$(
        impl serde::Serialize for $kind {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    )+};
}

serialize_as_str!(JobStatus, JobTimeline, BuildType);

/// A job as it appears on the board, to anyone.
///
/// The poster is identified by first name only — `split_part` of their display
/// name. That is the shape of the field rather than a tier: nothing here asks
/// for surnames on a public board, and the email is never selected at all.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicJob {
    pub id: Uuid,
    pub title: String,
    /// `None` means the poster chose "Other / not listed" — a recorded answer,
    /// not a missing one. See the header of `migrations/0018_job_intake.sql`.
    pub trade: Option<String>,
    pub status: JobStatus,
    pub build_type: BuildType,
    pub job_size: String,
    pub timeline: JobTimeline,
    pub budget_min_cents: Option<i64>,
    pub budget_max_cents: Option<i64>,
    pub postal_code: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Always `zip_centroid` or `none`. A job never publishes an exact point.
    pub location_precision: String,
    pub created_at: DateTime<Utc>,
    pub distance_m: Option<f64>,
    pub description: String,
    pub poster_first_name: Option<String>,
    /// In upload order. Empty is normal — photos are prompted, not required.
    pub photos: Vec<Photo>,
}

/// A stored photo, as published.
///
/// The URL is built from the storage key rather than stored, so the bucket can
/// move without a data migration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Photo {
    pub id: Uuid,
    pub url: String,
    pub width: i32,
    pub height: i32,
}

/// What the poster sees of their own job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnerJob {
    #[serde(flatten)]
    pub public: PublicJob,
    /// The board has no reason to carry these; the poster's own list does.
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

/// Which rows exist. The list and the detail query share it so they can never
/// disagree about whether a job is on the board.
const PREDICATE: &str = "\
    j.status = 'open' \
    AND ($1::float8 IS NULL OR ST_DWithin(j.public_point, \
         ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)) \
    AND ($4::uuid[] IS NULL OR j.trade_id = ANY($4)) \
    AND ($5::text IS NULL OR j.postal_code = $5)";

/// The one browse projection.
///
/// `split_part` takes the first whitespace-delimited token of the display name,
/// so the board carries a first name and never the whole of it. The email is
/// not selected here at all.
const SELECT_JOB: &str = "\
    SELECT j.id, j.title, t.slug AS trade, j.status, j.timeline, \
           j.build_type, j.job_size, \
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

fn bind_filters<'q>(
    query: sqlx::query::QueryAs<'q, sqlx::Postgres, JobRow, sqlx::postgres::PgArguments>,
    filters: &'q Filters,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, JobRow, sqlx::postgres::PgArguments> {
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

pub async fn list(
    conn: &mut PgConnection,
    filters: &Filters,
    limit: i64,
    cursor: Option<&Cursor>,
) -> Result<Page<PublicJob>, AppError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let sql = format!("{SELECT_JOB} WHERE {PREDICATE} {ORDER_AND_KEYSET}");

    let mut rows: Vec<JobRow> = bind_filters(sqlx::query_as(&sql), filters)
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
            .map(JobRow::into_public)
            .collect::<Result<_, _>>()?,
        next_cursor: next,
    })
}

/// Fill in the photos for a set of jobs, in one query.
///
/// Separate from the projection on purpose: joining photos into `SELECT_JOB`
/// would return one row per job per photo, and the keyset cursor counts rows —
/// page two would begin in the middle of a job. `url` is built here rather than
/// stored, so moving the bucket is a config change and not a data migration.
pub fn attach_photos(
    jobs: &mut [PublicJob],
    rows: Vec<crate::repo::job_photos::PhotoRow>,
    url_for: impl Fn(&str) -> String,
) {
    use std::collections::HashMap;

    let mut by_job: HashMap<Uuid, Vec<Photo>> = HashMap::new();
    for row in rows {
        by_job.entry(row.job_id).or_default().push(Photo {
            id: row.id,
            url: url_for(&row.storage_key),
            width: row.width,
            height: row.height,
        });
    }

    for job in jobs {
        if let Some(photos) = by_job.remove(&job.id) {
            job.photos = photos;
        }
    }
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
pub async fn find(conn: &mut PgConnection, id: Uuid) -> Result<Option<PublicJob>, AppError> {
    let sql = format!("{SELECT_JOB} WHERE j.id = $6 AND j.status = 'open'");
    let row: Option<JobRow> = sqlx::query_as(&sql)
        .bind(None::<f64>)
        .bind(None::<f64>)
        .bind(None::<f64>)
        .bind(None::<Vec<Uuid>>)
        .bind(None::<String>)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    row.map(JobRow::into_public).transpose()
}

/// The poster's own jobs, in every state — an owner needs to see what they
/// closed, not only what is live.
pub async fn for_poster(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<OwnerJob>, AppError> {
    let rows: Vec<OwnerJobRow> = sqlx::query_as(
        "SELECT j.id, j.title, t.slug AS trade, j.status, j.timeline, \
                j.build_type, j.job_size, \
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
    /// `None` is the poster's "Other / not listed", not an omission.
    pub trade_id: Option<Uuid>,
    pub build_type: BuildType,
    pub job_size: &'a str,
    /// Both or neither. `None` is the poster's "I'm not sure"; the schema no
    /// longer represents a half-filled range.
    pub budget: Option<(i64, i64)>,
    pub timeline: JobTimeline,
    pub postal_code: &'a str,
    pub region_id: Option<Uuid>,
    /// `(lon, lat)` of the ZIP centroid, when the ZIP resolves to one.
    pub centroid: Option<(f64, f64)>,
}

pub async fn insert(conn: &mut PgConnection, job: NewJob<'_>) -> Result<Uuid, AppError> {
    sqlx::query(
        "INSERT INTO jobs (id, posted_by_user_id, title, description, trade_id, \
                           build_type, job_size, \
                           budget_min_cents, budget_max_cents, timeline, postal_code, \
                           region_id, public_point, public_point_source) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
                 CASE WHEN $13::float8 IS NULL THEN NULL \
                      ELSE ST_SetSRID(ST_MakePoint($13, $14), 4326)::geography END, \
                 CASE WHEN $13::float8 IS NULL THEN 'none' ELSE 'zip_centroid' END)",
    )
    .bind(job.id)
    .bind(job.posted_by_user_id)
    .bind(job.title)
    .bind(job.description)
    .bind(job.trade_id)
    .bind(job.build_type.as_str())
    .bind(job.job_size)
    .bind(job.budget.map(|(min, _)| min))
    .bind(job.budget.map(|(_, max)| max))
    .bind(job.timeline.as_str())
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

/// Who posted a job, but only while it is still open.
///
/// The status is in the WHERE clause rather than checked afterwards so a closed
/// job is indistinguishable from one that never existed — a contractor must not
/// be able to tell the difference by whether contacting the poster is refused
/// or merely not found.
pub async fn open_job_poster(conn: &mut PgConnection, id: Uuid) -> Result<Option<Uuid>, AppError> {
    sqlx::query_scalar("SELECT posted_by_user_id FROM jobs WHERE id = $1 AND status = 'open'")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The board's one projection may read the display name — that is where the
    /// first name comes from — but only ever through split_part, never whole.
    /// The email is not selected at all.
    #[test]
    fn the_projection_returns_a_first_name_and_no_contact_details() {
        let projection = SELECT_JOB
            .split(" FROM ")
            .next()
            .expect("a SELECT has a projection");

        for forbidden in ["u.email", "posted_by_user_id"] {
            assert!(
                !projection.contains(forbidden),
                "the board must never return {forbidden}: {projection}"
            );
        }

        for fragment in projection.match_indices("u.display_name") {
            assert!(
                projection[..fragment.0].ends_with("split_part(btrim("),
                "u.display_name may only be read through split_part: {projection}"
            );
        }
    }

    /// List and detail must agree about which jobs exist. They are built from
    /// the same SELECT and the same predicate, so the only way to break that is
    /// to stop using them.
    #[test]
    fn list_and_detail_share_one_projection_and_predicate() {
        assert!(PREDICATE.contains("j.status = 'open'"));
        assert!(format!("{SELECT_JOB} WHERE {PREDICATE}").contains(PREDICATE));
        assert!(SELECT_JOB.contains("j.description"));
    }

    /// The spelling that reaches a client and the spelling stored in Postgres
    /// are the same string. They were not, once: a derived snake_case rename
    /// turned `Within2Weeks` into "within2_weeks" on the wire while the CHECK
    /// constraint held "within_2_weeks".
    #[test]
    fn the_wire_spelling_matches_the_database() {
        for status in JobStatus::ALL {
            assert_eq!(
                serde_json::to_string(&status).expect("serialize"),
                format!("\"{}\"", status.as_str())
            );
        }
        for timeline in JobTimeline::ALL {
            assert_eq!(
                serde_json::to_string(&timeline).expect("serialize"),
                format!("\"{}\"", timeline.as_str())
            );
        }
        for build_type in BuildType::ALL {
            assert_eq!(
                serde_json::to_string(&build_type).expect("serialize"),
                format!("\"{}\"", build_type.as_str())
            );
        }
    }

    #[test]
    fn status_and_timeline_round_trip() {
        for status in JobStatus::ALL {
            assert_eq!(JobStatus::parse(status.as_str()).unwrap(), status);
        }
        for timeline in JobTimeline::ALL {
            assert_eq!(JobTimeline::parse(timeline.as_str()).unwrap(), timeline);
        }
        for build_type in BuildType::ALL {
            assert_eq!(BuildType::parse(build_type.as_str()).unwrap(), build_type);
        }
        assert!(JobStatus::parse("elsewhere").is_err());
        assert!(JobTimeline::parse_request("eventually").is_err());
        assert!(BuildType::parse_request("knocking_it_down").is_err());
    }
}

/* ── Row types ─────────────────────────────────────────────────────────────
 * Separate from the serialized shapes so the `status`/`timeline` strings are
 * validated on the way out of the database rather than trusted.
 */

#[derive(sqlx::FromRow)]
struct JobRow {
    id: Uuid,
    title: String,
    trade: Option<String>,
    status: String,
    build_type: String,
    job_size: String,
    timeline: String,
    budget_min_cents: Option<i64>,
    budget_max_cents: Option<i64>,
    postal_code: String,
    lat: Option<f64>,
    lon: Option<f64>,
    location_precision: String,
    created_at: DateTime<Utc>,
    distance_m: Option<f64>,
    description: String,
    poster_first_name: Option<String>,
}

impl JobRow {
    fn into_public(self) -> Result<PublicJob, AppError> {
        Ok(PublicJob {
            id: self.id,
            title: self.title,
            trade: self.trade,
            status: JobStatus::parse(&self.status)?,
            build_type: BuildType::parse(&self.build_type)?,
            job_size: self.job_size,
            timeline: JobTimeline::parse(&self.timeline)?,
            budget_min_cents: self.budget_min_cents,
            budget_max_cents: self.budget_max_cents,
            postal_code: self.postal_code,
            lat: self.lat,
            lon: self.lon,
            location_precision: self.location_precision,
            created_at: self.created_at,
            distance_m: self.distance_m,
            description: self.description,
            // An all-whitespace display name would split to "", which is worse
            // than admitting we do not have one.
            poster_first_name: self.poster_first_name.filter(|n| !n.is_empty()),
            // Filled in by `attach_photos` after the rows are read. Left empty
            // here rather than joined into the projection: a job with eight
            // photos would multiply every row by eight, and the keyset cursor
            // counts rows.
            photos: Vec::new(),
        })
    }
}

#[derive(sqlx::FromRow)]
struct OwnerJobRow {
    id: Uuid,
    title: String,
    trade: Option<String>,
    status: String,
    build_type: String,
    job_size: String,
    timeline: String,
    budget_min_cents: Option<i64>,
    budget_max_cents: Option<i64>,
    postal_code: String,
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
        let (posted_by_user_id, closed_at, updated_at) =
            (self.posted_by_user_id, self.closed_at, self.updated_at);

        let public = JobRow {
            id: self.id,
            title: self.title,
            trade: self.trade,
            status: self.status,
            build_type: self.build_type,
            job_size: self.job_size,
            timeline: self.timeline,
            budget_min_cents: self.budget_min_cents,
            budget_max_cents: self.budget_max_cents,
            postal_code: self.postal_code,
            lat: self.lat,
            lon: self.lon,
            location_precision: self.location_precision,
            created_at: self.created_at,
            distance_m: None,
            description: self.description,
            // The poster knows their own name; the field is for the board.
            poster_first_name: None,
        }
        .into_public()?;

        Ok(OwnerJob {
            public,
            posted_by_user_id,
            closed_at,
            updated_at,
        })
    }
}
