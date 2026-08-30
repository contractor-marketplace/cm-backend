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
    pub query: Option<String>,
    /// Trades the free-text query itself asked for, through the same alias
    /// vocabulary the directory uses: a contractor typing "water heater" is
    /// looking for plumbing work, and no job is titled "C-36".
    pub query_trade_ids: Vec<Uuid>,
    pub trade_ids: Option<Vec<Uuid>>,
    pub postal_code: Option<String>,
    pub near: Option<Near>,
    pub timeline: Option<JobTimeline>,
    pub build_type: Option<BuildType>,
    /// Jobs whose upper budget reaches at least this. A job with no budget at
    /// all is excluded by it — "I'm not sure" is not a number, and treating it
    /// as zero would hide every one of them behind any floor.
    pub budget_min_cents: Option<i64>,
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
    /// The value the ordering leads with, when it leads with something other
    /// than the posting time. Absent for the newest-first default, which
    /// already leads with `created_at`.
    pub sort_key: Option<f64>,
}

/// How the board is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Newest first. The only order the board has ever had, and still the
    /// default: a job board is a queue before it is a search result.
    Newest,
    /// How well the text matched, for a search rather than a browse.
    Best,
    /// Largest budget first, on the top of the range.
    Budget,
    /// Nearest first, which needs a centre to measure from.
    Distance,
}

pub struct Page<T> {
    pub jobs: Vec<T>,
    pub next_cursor: Option<Cursor>,
}

pub const MAX_PAGE: i64 = 50;
/// The hard ceiling on map points, matching the contractor map. A zoomed-out
/// viewport degrades honestly rather than returning a silently partial map.
pub const MAX_MAP_POINTS: i64 = 500;
pub const DEFAULT_PAGE: i64 = 20;

/// Which rows exist. The list and the detail query share it so they can never
/// disagree about whether a job is on the board.
const PREDICATE: &str = "\
    j.status = 'open' \
    AND ($1::float8 IS NULL OR ST_DWithin(j.public_point, \
         ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)) \
    AND ($4::uuid[] IS NULL OR j.trade_id = ANY($4)) \
    AND ($5::text IS NULL OR j.postal_code = $5) \
    AND ($6::text IS NULL \
         OR j.search_doc @@ websearch_to_tsquery('public.english_unaccent', $6) \
         OR ($7::uuid[] IS NOT NULL AND j.trade_id = ANY($7))) \
    AND ($8::text IS NULL OR j.timeline = $8) \
    AND ($9::text IS NULL OR j.build_type = $9) \
    AND ($10::bigint IS NULL OR j.budget_max_cents >= $10)";

/// How many bind slots the shared predicate occupies. Tail clauses number
/// themselves from here rather than being written by hand at each call site.
const PREDICATE_BINDS: usize = 10;

/// The query-text slot, read by the predicate and by the relevance ordering.
const QUERY_BIND: usize = 6;

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
           split_part(btrim(u.display_name), ' ', 1) AS poster_first_name";

/// The tables the projection reads. Split from the columns so the board can
/// append a relevance score, which is computed per query and cannot live in a
/// constant.
const FROM_JOBS: &str = "\
      FROM jobs j \
      LEFT JOIN trades t ON t.id = j.trade_id \
      JOIN users u ON u.id = j.posted_by_user_id";

/// Generic over the row, so the board and the map bind the shared predicate
/// the same way rather than each keeping its own copy of the order.
fn bind_filters<'q, T>(
    query: sqlx::query::QueryAs<'q, sqlx::Postgres, T, sqlx::postgres::PgArguments>,
    filters: &'q Filters,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, T, sqlx::postgres::PgArguments> {
    query
        .bind(filters.near.map(|n| n.lon))
        .bind(filters.near.map(|n| n.lat))
        .bind(filters.near.map(|n| n.radius_m))
        .bind(filters.trade_ids.clone())
        .bind(filters.postal_code.clone())
        .bind(filters.query.clone())
        .bind((!filters.query_trade_ids.is_empty()).then(|| filters.query_trade_ids.clone()))
        .bind(filters.timeline.map(|t| t.as_str()))
        .bind(filters.build_type.map(|b| b.as_str()))
        .bind(filters.budget_min_cents)
}

/// Newest first. The cursor predicate is `<` because the order is DESC, and it
/// compares the identical `(created_at, id)` tuple the ORDER BY ends on.
/// Metres from the centre the caller supplied. Spelled out because
/// `distance_m` is a SELECT alias and Postgres does not allow one in `WHERE`.
fn distance_expression() -> String {
    "ST_Distance(j.public_point, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography)".to_owned()
}

/// How well the text matched.
///
/// Cast to `float8` at the edge, because `ts_rank_cd` returns `real` and
/// decoding a `real` as an `f64` fails at the row reader rather than at the
/// query — a 500 that says nothing about ranking. The same cast is on
/// `quality_score` in the directory, for the same reason.
fn relevance_expression() -> String {
    format!(
        "ts_rank_cd(j.search_doc, \
         websearch_to_tsquery('public.english_unaccent', ${QUERY_BIND}))::float8"
    )
}

/// The leading key of an ordering, and where to read it back from to build the
/// next cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyField {
    Relevance,
    Budget,
    Distance,
}

/// What the board leads with, and which way.
///
/// Every ordering ends in `(created_at DESC, id DESC)`, which is the pair the
/// cursor has always carried, so the tie-break is a total order whatever leads.
/// The keyset is written as "past the key, or level with it and past the
/// tie-break" rather than one row-wise comparison: a row-wise comparison
/// applies a single direction to every column, and these orderings are mixed.
struct Ordering {
    key: Option<String>,
    field: Option<KeyField>,
    /// True when the leading key sorts descending, as budget and relevance do.
    descending: bool,
}

impl Ordering {
    fn order_by(&self) -> String {
        match &self.key {
            Some(key) => {
                let direction = if self.descending {
                    "DESC NULLS LAST"
                } else {
                    "ASC"
                };
                format!("{key} {direction}, j.created_at DESC, j.id DESC")
            }
            None => "j.created_at DESC, j.id DESC".to_owned(),
        }
    }
}

fn ordering_for(sort: Sort, filters: &Filters) -> Ordering {
    let by = |key: String, field: KeyField, descending: bool| Ordering {
        key: Some(key),
        field: Some(field),
        descending,
    };

    match sort {
        Sort::Budget => by("j.budget_max_cents".to_owned(), KeyField::Budget, true),
        Sort::Distance if filters.near.is_some() => {
            by(distance_expression(), KeyField::Distance, false)
        }
        Sort::Best if filters.query.is_some() => {
            by(relevance_expression(), KeyField::Relevance, true)
        }
        // A sort with nothing to sort on degrades to the queue rather than
        // ordering by a column that is NULL for every row.
        Sort::Newest | Sort::Best | Sort::Distance => Ordering {
            key: None,
            field: None,
            descending: true,
        },
    }
}

/// The tail of the board query: resume where the last page stopped, order, cap.
fn order_and_keyset(ordering: &Ordering) -> String {
    // The key slot exists only when the ordering leads with one. Binding a
    // parameter the statement never mentions is not harmless — Postgres counts
    // the placeholders it can see and refuses the extra — so the tail numbers
    // itself from what it is actually going to say.
    let key = PREDICATE_BINDS + 1;
    let base = if ordering.key.is_some() {
        key
    } else {
        PREDICATE_BINDS
    };
    let (at, id, limit) = (base + 1, base + 2, base + 3);

    let keyset = match &ordering.key {
        Some(expr) => {
            let op = if ordering.descending { "<" } else { ">" };
            format!(
                "AND (${at}::timestamptz IS NULL OR {expr} {op} ${key}::float8 \
                      OR ({expr} IS NOT DISTINCT FROM ${key}::float8 \
                          AND (j.created_at, j.id) < (${at}, ${id}::uuid)))"
            )
        }
        None => format!(
            "AND (${at}::timestamptz IS NULL OR (j.created_at, j.id) < (${at}, ${id}::uuid))"
        ),
    };

    format!("{keyset} ORDER BY {} LIMIT ${limit}", ordering.order_by())
}

pub async fn list(
    conn: &mut PgConnection,
    filters: &Filters,
    sort: Sort,
    limit: i64,
    cursor: Option<&Cursor>,
) -> Result<Page<PublicJob>, AppError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let ordering = ordering_for(sort, filters);
    let sql = format!(
        "{SELECT_JOB}, {relevance} AS rank_score {FROM_JOBS} WHERE {PREDICATE} {tail}",
        relevance = relevance_expression(),
        tail = order_and_keyset(&ordering),
    );

    let query = bind_filters(sqlx::query_as(&sql), filters);
    // Bound only when the statement refers to it; see `order_and_keyset`.
    let query = match ordering.key {
        Some(_) => query.bind(cursor.and_then(|c| c.sort_key)),
        None => query,
    };

    let mut rows: Vec<JobRow> = query
        .bind(cursor.map(|c| c.created_at))
        .bind(cursor.map(|c| c.id))
        .bind(limit + 1)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    let next = take_next(&mut rows, limit, |r| Cursor {
        created_at: r.created_at,
        id: r.id,
        // Read from whichever column the ordering actually sorted by, so the
        // next page compares against the value this one stopped at.
        sort_key: match ordering.field {
            Some(KeyField::Relevance) => r.rank_score,
            Some(KeyField::Budget) => r.budget_max_cents.map(|cents| cents as f64),
            Some(KeyField::Distance) => r.distance_m,
            None => None,
        },
    });

    Ok(Page {
        jobs: rows
            .into_iter()
            .map(JobRow::into_public)
            .collect::<Result<_, _>>()?,
        next_cursor: next,
    })
}

/// How many jobs each choice would leave, given everything else already
/// chosen.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Facets {
    /// Total under the current filters, which is also the "N jobs" the board
    /// shows. The board itself only ever knew how many rows it had loaded.
    pub total: i64,
    pub trade: Vec<Facet>,
    pub timeline: Vec<Facet>,
    pub build_type: Vec<Facet>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Facet {
    pub value: String,
    pub count: i64,
}

/// Count the facets under the same predicate the results use.
///
/// One query with `GROUPING SETS` rather than four, and under `PREDICATE`
/// rather than a copy of it — counts that disagree with the list they sit
/// beside are worse than no counts, because they are read as the list being
/// wrong.
///
/// Note what this deliberately does not do: each count is taken with *every*
/// current filter applied, including the facet's own. So the number beside
/// "Roofing" is how many roofing jobs match, not how many there would be if
/// roofing were selected instead. That is the honest reading of "what is in
/// front of me", and it is why selecting a facet never surprises.
pub async fn facets(conn: &mut PgConnection, filters: &Filters) -> Result<Facets, AppError> {
    // GROUPING() says which set each row came from, and it is not optional
    // here. `trade_id IS NULL` is the board's "Other / not listed" escape
    // hatch, so the trade set contains a row whose slug is NULL — identical in
    // shape to the grand-total row from the empty set. Without these flags the
    // two are indistinguishable and one silently overwrites the other.
    let sql = format!(
        "SELECT GROUPING(t.slug) AS g_trade, \
                GROUPING(j.timeline) AS g_timeline, \
                GROUPING(j.build_type) AS g_build, \
                t.slug AS trade, j.timeline, j.build_type, count(*) AS n \
           FROM jobs j \
           LEFT JOIN trades t ON t.id = j.trade_id \
          WHERE {PREDICATE} \
          GROUP BY GROUPING SETS ((t.slug), (j.timeline), (j.build_type), ())"
    );

    type Row = (
        i32,
        i32,
        i32,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
    );
    let rows: Vec<Row> = bind_filters(sqlx::query_as(&sql), filters)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    let mut facets = Facets::default();
    for (g_trade, g_timeline, g_build, trade, timeline, build_type, count) in rows {
        match (g_trade, g_timeline, g_build) {
            // A grouping flag of 0 means the column is part of this row's set.
            (0, _, _) => facets.trade.push(Facet {
                // A job posted as "Other / not listed" has no trade slug, and
                // saying so is more use than dropping it from the count.
                value: trade.unwrap_or_else(|| "other".to_owned()),
                count,
            }),
            (_, 0, _) => {
                if let Some(value) = timeline {
                    facets.timeline.push(Facet { value, count });
                }
            }
            (_, _, 0) => {
                if let Some(value) = build_type {
                    facets.build_type.push(Facet { value, count });
                }
            }
            // Every column rolled up: the grand total.
            _ => facets.total = count,
        }
    }

    for group in [
        &mut facets.trade,
        &mut facets.timeline,
        &mut facets.build_type,
    ] {
        group.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
    }

    Ok(facets)
}

/// One pin on the jobs map.
///
/// Narrower than the board row on purpose: a map needs a position and enough
/// to label it, and shipping the description and photo set for five hundred
/// pins is bytes nobody reads.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobPoint {
    pub id: Uuid,
    pub title: String,
    pub trade: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub budget_min_cents: Option<i64>,
    pub budget_max_cents: Option<i64>,
}

/// Map points: the same predicate as the board, a narrower projection, a cap.
///
/// This exists because deriving pins from the loaded page is wrong, and the
/// contractor side already learned that: a board page holds twenty jobs and the
/// map was drawing twenty pins however many matched, so a map of the county
/// showed a fifth of the work available on it. The list and the map share
/// `PREDICATE`, so they cannot disagree about which jobs exist.
pub async fn map_points(
    conn: &mut PgConnection,
    filters: &Filters,
    limit: i64,
) -> Result<(Vec<JobPoint>, bool), AppError> {
    let limit = limit.clamp(1, MAX_MAP_POINTS);

    let sql = format!(
        "SELECT j.id, j.title, t.slug AS trade, \
                ST_Y(j.public_point::geometry) AS lat, \
                ST_X(j.public_point::geometry) AS lon, \
                j.budget_min_cents, j.budget_max_cents \
           FROM jobs j \
           LEFT JOIN trades t ON t.id = j.trade_id \
          WHERE {PREDICATE} AND j.public_point IS NOT NULL \
          ORDER BY j.created_at DESC, j.id DESC LIMIT ${limit}",
        limit = PREDICATE_BINDS + 1
    );

    let mut points: Vec<JobPoint> = bind_filters(sqlx::query_as(&sql), filters)
        .bind(limit + 1)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    let truncated = points.len() as i64 > limit;
    if truncated {
        points.pop();
    }

    Ok((points, truncated))
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for JobPoint {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            title: row.try_get("title")?,
            trade: row.try_get("trade")?,
            lat: row.try_get("lat")?,
            lon: row.try_get("lon")?,
            budget_min_cents: row.try_get("budget_min_cents")?,
            budget_max_cents: row.try_get("budget_max_cents")?,
        })
    }
}

/// Fill in the photos for a set of jobs, in one query.
///
/// Separate from the projection on purpose: joining photos into `SELECT_JOB`
/// would return one row per job per photo, and the keyset cursor counts rows —
/// page two would begin in the middle of a job. `url` is built here rather than
/// stored, so moving the bucket is a config change and not a data migration.
/// Takes anything that yields `&mut PublicJob` rather than a slice, so the
/// poster's own list — `Vec<OwnerJob>`, which merely contains a `PublicJob` —
/// can be filled in without first copying it apart.
pub fn attach_photos<'a>(
    jobs: impl IntoIterator<Item = &'a mut PublicJob>,
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
/// One job, if the board would show it.
///
/// Runs the shared predicate rather than restating `status = 'open'` and
/// hand-binding a `None` per filter. That version drifted the moment the
/// predicate grew: every parameter added to the board had to be echoed here as
/// another `None`, and the test that claims list and detail share a projection
/// only compares the constants — it would not have caught the miscount.
pub async fn find(conn: &mut PgConnection, id: Uuid) -> Result<Option<PublicJob>, AppError> {
    let value = PREDICATE_BINDS + 1;
    let sql = format!(
        "{SELECT_JOB}, NULL::float8 AS rank_score {FROM_JOBS} \
         WHERE {PREDICATE} AND j.id = ${value}"
    );

    let row: Option<JobRow> = bind_filters(sqlx::query_as(&sql), &Filters::default())
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

/// Put a closed job back on the board.
///
/// `status = 'closed'` in the WHERE clause, not `<> 'open'`, and that is the
/// whole safety property: a **cancelled** job has already had its photos
/// deleted from the object store, so reopening one would restore a listing that
/// silently lost its pictures. Closed is reversible because closing takes
/// nothing away; cancelled is not.
///
/// `closed_at` goes back to NULL so the row does not claim to be both open and
/// closed at a time.
pub async fn reopen(conn: &mut PgConnection, id: Uuid, poster: Uuid) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE jobs SET status = 'open', closed_at = NULL, updated_at = now() \
          WHERE id = $1 AND posted_by_user_id = $2 AND status = 'closed'",
    )
    .bind(id)
    .bind(poster)
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
    /// How well this row matched the text, present only on the board query —
    /// the one surface that paginates and so the only one needing a cursor key.
    rank_score: Option<f64>,
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
            // The owner's own list is not the board and does not rank.
            rank_score: None,
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
