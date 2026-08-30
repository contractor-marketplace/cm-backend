//! Contractor search.
//!
//! Every query here reads `public_point` and never `precise_point`. That is not
//! a display convention: if distance search ran against the precise point while
//! the map published a centroid, the radius filter could be binary-searched to
//! recover the address the centroid was protecting.
//!
//! Results are keyset-paginated. `OFFSET` on a large table both scans what it
//! skips and duplicates rows when the underlying data shifts between pages.

use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

use super::contractors::{PublicContractor, PublicPointSource};

/// A geographic centre and radius, in metres.
#[derive(Debug, Clone, Copy)]
pub struct Near {
    pub lat: f64,
    pub lon: f64,
    pub radius_m: f64,
}

/// A viewport.
#[derive(Debug, Clone, Copy)]
pub struct BoundingBox {
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Text relevance blended with standing quality. The default.
    Best,
    /// Standing quality alone, ignoring how well the text matched.
    Rating,
    Distance,
    Name,
}

/// Where the previous page ended. Encoded opaquely at the edge.
///
/// `sort_key` is the value of whatever the ordering leads with — the blended
/// rank, the quality score, the distance — and is absent only for the plain
/// alphabetical sort, which leads with the name already.
///
/// It has to be here. The previous cursor carried only `(name, id)` while the
/// ORDER BY led with distance or relevance, so page two filtered on a column it
/// was not ordered by and silently dropped rows. The front end worked around it
/// by refusing to paginate those sorts at all.
#[derive(Debug, Clone)]
pub struct Cursor {
    pub name: String,
    pub id: Uuid,
    pub sort_key: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub query: Option<String>,
    pub trade_ids: Vec<Uuid>,
    pub verified_only: bool,
    pub postal_code: Option<String>,
    pub near: Option<Near>,
    pub bbox: Option<BoundingBox>,
    /// Trades the free-text query itself asked for.
    ///
    /// Distinct from `trade_ids`, which is the filter a caller set explicitly.
    /// These come from resolving `query` through the alias vocabulary, and they
    /// widen the text match rather than narrowing the result set: "hvac" should
    /// find heating contractors *as well as* anything named "HVAC", not
    /// intersect the two.
    pub query_trade_ids: Vec<Uuid>,
}

/// The similarity a query word must reach against a business name for the
/// fuzzy fallback to fire.
///
/// pg_trgm defaults this to 0.6, which is too strict to be useful: measured
/// against 40 real business names from the CSLB register, each with one
/// character deleted, 0.6 found 7 of them and 0.5 found all 40. Nonsense
/// queries match nothing at either setting, or at anything down to 0.35 — the
/// margin is wide, so 0.5 is the conservative end of a broad plateau rather
/// than a value tuned to the edge.
///
/// Applied per connection by `pool::connect`, because the underlying
/// `pg_trgm.word_similarity_threshold` is session state and the `<%` operator
/// reads it rather than taking a threshold inline. Anything that searches
/// outside that pool has to set it too, or it silently searches more strictly
/// than production does.
pub const WORD_SIMILARITY_THRESHOLD: f64 = 0.5;

/// The hard ceiling on a page, whatever the caller asks for.
pub const MAX_PAGE: i64 = 50;
/// The hard ceiling on map points. A zoomed-out viewport degrades honestly
/// rather than returning a silently partial map.
pub const MAX_MAP_POINTS: i64 = 500;

/// How many bind slots the shared predicate occupies.
///
/// Every caller appends its own parameters after these, and each one used to
/// hand-write `$12`, `$13`, `$14`. Four call sites renumbering by hand is how a
/// cursor silently starts binding a radius, so the tail clauses are built from
/// this constant instead. Adding a filter means editing `PREDICATE`,
/// `bind_filters` and this number — never a call site.
///
/// New filters append: `$1`–`$11` keep their meanings, which is what lets
/// `SELECT` reference the centre as `$1`/`$2` and the relevance ordering
/// reference the query text as `$4` without either of them moving.
const PREDICATE_BINDS: usize = 12;

/// The query-text slot. Referenced by the predicate and, separately, by the
/// relevance ordering — a coupling `the_relevance_ordering_reads_the_query_bind`
/// pins so the two cannot drift apart.
const QUERY_BIND: usize = 4;

/// The centre the caller measures from. Read by the projection's distance
/// column and by the distance ordering, which have to agree.
const NEAR_LON_BIND: usize = 1;
const NEAR_LAT_BIND: usize = 2;

/// One shared WHERE clause, so list and map can never disagree about what
/// matches — a map showing pins the list omits is a bug report nobody can
/// reproduce.
///
/// "Did the query route to a trade this contractor holds" is asked here as a
/// correlated `EXISTS`, and again by the ranking. Hoisting it into a lateral
/// join to ask once looks like an obvious win and measured as one in isolation
/// — and was 25% *slower* through the real endpoint, because the planner can
/// short-circuit an `EXISTS` per row and cannot skip a join it has already
/// built. The isolated benchmark differed from the shipped query in one detail,
/// an inlined scalar subquery for the trade id, and that was enough to change
/// the plan. Left as it is, on the measurement that matches production.
///
/// The fuzzy clause is `<%` (word similarity), not `%` (whole-string
/// similarity), and the difference is the whole feature. `%` scores the query
/// against the entire column: "ibara" against "Ibarra & Daughters
/// Construction" scores 0.161 against a 0.3 threshold, so it does not match —
/// and neither does any other typo in any other multi-word business name,
/// which is nearly all of them. `<%` scores against the closest *word* and
/// gives 0.667, so it does. Both use the same `contractors_name_trgm` GIN
/// index; the plan is a bitmap index scan either way.
const PREDICATE: &str = "\
    ($1::float8 IS NULL OR ST_DWithin(c.public_point, \
        ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)) \
    AND ($4::text IS NULL \
         OR c.search_doc @@ websearch_to_tsquery('public.english_unaccent', $4) \
         OR $4 <% c.display_name \
         OR ($12::uuid[] IS NOT NULL AND EXISTS ( \
                SELECT 1 FROM contractor_trades qt \
                 WHERE qt.contractor_id = c.id AND qt.trade_id = ANY($12)))) \
    AND (NOT $5::bool OR c.verified) \
    AND ($6::uuid[] IS NULL OR EXISTS ( \
            SELECT 1 FROM contractor_trades ct \
             WHERE ct.contractor_id = c.id AND ct.trade_id = ANY($6))) \
    AND ($7::text IS NULL OR COALESCE(c.owner_address_postal_code, c.postal_code) = $7) \
    AND ($8::float8 IS NULL OR c.public_point && \
         ST_MakeEnvelope($8, $9, $10, $11, 4326)::geography)";

fn bind_filters<'q>(
    query: sqlx::query::QueryAs<'q, sqlx::Postgres, PublicContractor, sqlx::postgres::PgArguments>,
    filters: &'q Filters,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, PublicContractor, sqlx::postgres::PgArguments> {
    query
        .bind(filters.near.map(|near| near.lon))
        .bind(filters.near.map(|near| near.lat))
        .bind(filters.near.map(|near| near.radius_m))
        .bind(filters.query.as_deref())
        .bind(filters.verified_only)
        .bind((!filters.trade_ids.is_empty()).then(|| filters.trade_ids.clone()))
        .bind(filters.postal_code.as_deref())
        .bind(filters.bbox.map(|b| b.min_lon))
        .bind(filters.bbox.map(|b| b.min_lat))
        .bind(filters.bbox.map(|b| b.max_lon))
        .bind(filters.bbox.map(|b| b.max_lat))
        .bind((!filters.query_trade_ids.is_empty()).then(|| filters.query_trade_ids.clone()))
}

/// The projection. `precise_point` is absent by construction.
///
/// The address comes from `license_records`, not from anything a contractor
/// typed: it is the address on the licence, which the CSLB register publishes.
/// A listing that asked to be kept off the map (`address_visibility` of
/// 'protected') is still returned here with its address — the register still
/// publishes it, and pretending otherwise would be theatre. What 'protected'
/// changes is the published POINT, which is what `location::republish` decides.
const SELECT: &str = "\
    SELECT c.id, c.slug, c.display_name, c.verified, c.verified_at, c.bio, \
           c.website_url, c.public_phone, c.accepts_dm, \
           (c.claimed_by_user_id IS NOT NULL) AS is_claimed, \
           ST_Y(c.public_point::geometry) AS lat, \
           ST_X(c.public_point::geometry) AS lon, \
           c.public_point_source, \
           COALESCE(c.owner_address_postal_code, c.postal_code) AS postal_code, \
           l.license_no, l.status AS license_status, \
           COALESCE(c.owner_address_line1, l.address_line1) AS address_line1, \
           COALESCE(c.owner_address_city, l.city) AS address_city, \
           COALESCE(c.owner_address_state, l.state) AS address_state, \
           (c.owner_address_line1 IS NOT NULL) AS address_is_owner_supplied, \
           l.address_line1 AS license_address_line1, l.city AS license_address_city, \
           c.google_review_url, c.yelp_url, \
           c.photo_storage_key, c.photo_width, c.photo_height, \
           COALESCE(( \
               SELECT array_agg(DISTINCT t.slug ORDER BY t.slug) \
                 FROM contractor_trades ct JOIN trades t ON t.id = ct.trade_id \
                WHERE ct.contractor_id = c.id), '{}') AS trades, \
           CASE WHEN $1::float8 IS NULL THEN NULL \
                ELSE ST_Distance(c.public_point, \
                     ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography) END AS distance_m, \
           c.google_rating::float8 AS google_rating, c.google_review_count, \
           c.google_place_url, c.quality_score::float8 AS quality_score";

/// The tables the projection reads. Split from the columns so a caller can add
/// one — the blended rank is computed per query and cannot live in a constant.
const FROM: &str = "\
      FROM contractors c \
      LEFT JOIN license_records l ON l.id = c.license_record_id";

/// Turn the stored object keys into public URLs.
///
/// Separate from the query for the same reason `jobs::attach_photos` is: the
/// row reader has no access to the object store, and giving it one would put a
/// deployment concern inside a `FromRow`. Every read path that serves a
/// contractor to a client must call this, or profile photos go out as bare
/// storage keys.
pub fn attach_photo_urls(contractors: &mut [PublicContractor], url_for: impl Fn(&str) -> String) {
    for contractor in contractors {
        if let Some(key) = contractor.photo_url.take() {
            contractor.photo_url = Some(url_for(&key));
        }
    }
}

/// A page of results plus the cursor for the next one.
#[derive(Debug)]
pub struct Page {
    pub contractors: Vec<PublicContractor>,
    pub next_cursor: Option<Cursor>,
}

/* ── SQL assembly ──────────────────────────────────────────────────────────
 * Each read path is built by one of these rather than by a `format!` inlined
 * at the call site, so a test can read the finished statement. That is what
 * makes "every read path filters through PREDICATE" an assertion rather than a
 * convention, and it is where the tail bind numbers come from.
 */

/// The list statement. Tail binds: cursor sort key, cursor name, cursor id, limit.
fn list_sql(ordering: &Ordering) -> String {
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
    let (name, id, limit) = (base + 1, base + 2, base + 3);
    let op = ordering.direction.comparison();
    let cast = ordering.field.map(KeyField::cast).unwrap_or("float8");

    // Written as "past the key, or level with it and past the tie-break"
    // rather than as one row-wise comparison, because a row-wise comparison
    // applies a single direction to every column. The ordering is
    // `key DESC, display_name ASC, id ASC` — mixed — and
    // `(key, name, id) < (…)` would silently mean `name DESC` too, excluding
    // every row alphabetically after the cursor. That returns a short page
    // that looks like a page.
    let keyset = match &ordering.key {
        Some(expr) => format!(
            "AND (${name}::text IS NULL OR {expr} {op} ${key}::{cast} \
                  OR ({expr} = ${key}::{cast} \
                      AND (c.display_name, c.id) > (${name}, ${id}::uuid)))"
        ),
        None => format!(
            "AND (${name}::text IS NULL OR (c.display_name, c.id) > (${name}, ${id}::uuid))"
        ),
    };

    format!(
        "{SELECT}, {rank} AS rank_score {FROM} \
         WHERE {PREDICATE} {keyset} ORDER BY {order} LIMIT ${limit}",
        rank = rank_expression(),
        order = ordering.order_by(),
    )
}

/// The map statement: the same predicate, a narrower ordering, one tail bind.
fn map_sql() -> String {
    let limit = PREDICATE_BINDS + 1;
    format!(
        "{SELECT} {FROM} WHERE {PREDICATE} AND c.public_point IS NOT NULL \
         ORDER BY c.quality_score DESC, c.display_name, c.id LIMIT ${limit}"
    )
}

/// The detail statement, narrowed by `c.id` or `c.slug`.
///
/// It runs the full predicate against a default `Filters`, so detail and list
/// agree about what a visitor may see: a row the list would exclude is not
/// reachable by guessing its slug.
fn find_sql(column: &str) -> String {
    let value = PREDICATE_BINDS + 1;
    format!("{SELECT} {FROM} WHERE {PREDICATE} AND c.{column} = ${value}")
}

/// Metres from the centre the caller supplied.
///
/// Spelled out rather than referred to as `distance_m`, because that is a
/// SELECT alias and Postgres does not allow one in `WHERE` — the keyset needs
/// the expression itself. It is the same arithmetic the projection does, minus
/// the null guard, since distance is only ever the sort key when a centre was
/// given.
fn distance_expression() -> String {
    format!(
        "ST_Distance(c.public_point, \
         ST_SetSRID(ST_MakePoint(${NEAR_LON_BIND}, ${NEAR_LAT_BIND}), 4326)::geography)"
    )
}

/// How much the standing quality score counts against text relevance.
///
/// Small on purpose. A text match scores at least 1.0 from the match bonus
/// below, so quality can reorder listings that matched but can never lift one
/// that did not above one that did: somebody searching "ibarra" wants Ibarra,
/// not the best-rated builder in the county.
const QUALITY_WEIGHT: f64 = 0.5;

/// What the directory ranks by, as SQL.
///
/// Four terms, ordered by how much they actually tell you:
///
///   * **The text matched** — 1.0. Somebody who types a business name wants
///     that business, and nothing below should outrank it.
///   * **This is that kind of contractor** — 0.75. The query resolved through
///     the trade vocabulary and this listing holds the licence class. Weaker
///     than naming the business, stronger than anything else, because it is a
///     fact about the licence rather than a coincidence about the spelling.
///   * **The name is close** — 0.5. Enough to surface a typo, and deliberately
///     ranked below both of the above: "solar" is one letter from "Polar", and
///     an actual solar contractor should beat an air-conditioning company whose
///     name nearly rhymes.
///   * **How well, and how good** — `ts_rank_cd` separates text matches from
///     each other, and the standing quality score orders equals.
///
/// With no query the first four are zero and the whole thing is quality, which
/// is what turns browsing from alphabetical into best-first.
///
/// Distance is deliberately not a term. Every result inside a radius filter is
/// already near enough; mixing distance into the blend would mean a slightly
/// closer, slightly worse contractor outranks a better one for reasons the
/// visitor cannot see. `sort=distance` remains available and says plainly what
/// it does.
fn rank_expression() -> String {
    format!(
        "(CASE WHEN ${QUERY_BIND}::text IS NULL THEN 0.0 ELSE \
             ts_rank_cd(c.search_doc, \
                 websearch_to_tsquery('public.english_unaccent', ${QUERY_BIND})) \
             + CASE WHEN c.search_doc @@ \
                 websearch_to_tsquery('public.english_unaccent', ${QUERY_BIND}) \
                    THEN 1.0 ELSE 0.0 END \
             + CASE WHEN ${PREDICATE_BINDS}::uuid[] IS NOT NULL AND EXISTS ( \
                     SELECT 1 FROM contractor_trades rt \
                      WHERE rt.contractor_id = c.id \
                        AND rt.trade_id = ANY(${PREDICATE_BINDS})) \
                    THEN 0.75 ELSE 0.0 END \
             + CASE WHEN ${QUERY_BIND} <% c.display_name THEN 0.5 ELSE 0.0 END \
         END + {QUALITY_WEIGHT} * c.quality_score)::float8"
    )
}

/// Which column of the returned row holds the value the cursor resumes from.
///
/// The ordering and the cursor have to name the same number, and the ordering
/// does not always sort by the expression the projection calls `rank_score` —
/// see `ordering_for`, where a browse with no query sorts by the bare quality
/// column so the index can serve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyField {
    Rank,
    Quality,
    Distance,
}

impl KeyField {
    /// What to cast the cursor's bind to. `quality_score` is `real`; comparing
    /// it against a `float8` promotes the column and loses the index, which is
    /// the whole reason the browse ordering was rewritten.
    fn cast(self) -> &'static str {
        match self {
            Self::Quality => "real",
            Self::Rank | Self::Distance => "float8",
        }
    }
}

/// Which direction a sort runs, and therefore which way its cursor compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Best first: the cursor takes rows strictly *below* where it stopped.
    Descending,
    /// Nearest or first alphabetically: strictly *above*.
    Ascending,
}

impl Direction {
    fn sql(self) -> &'static str {
        match self {
            Self::Descending => "DESC",
            Self::Ascending => "ASC",
        }
    }

    fn comparison(self) -> &'static str {
        match self {
            Self::Descending => "<",
            Self::Ascending => ">",
        }
    }
}

/// The complete ordering for a sort: what it leads with, which way, and the
/// expression a cursor has to compare against to resume it.
///
/// Keyset pagination is only well-defined over a total order, so every ordering
/// ends in `(display_name, id)` and every cursor carries that pair. What
/// changes is the leading key, and the whole point of returning it here is that
/// the ORDER BY and the cursor comparison are built from the *same* string and
/// cannot disagree — which is exactly how page two used to lose rows.
struct Ordering {
    /// The leading key, or `None` for the plain alphabetical sort.
    key: Option<String>,
    /// Where to read that key back from, to build the next cursor.
    field: Option<KeyField>,
    direction: Direction,
}

impl Ordering {
    fn order_by(&self) -> String {
        match &self.key {
            Some(key) => format!("{key} {}, c.display_name, c.id", self.direction.sql()),
            None => "c.display_name, c.id".to_owned(),
        }
    }
}

/// A sort the caller cannot support degrades to the stable key rather than
/// ordering by a column that is NULL for every row.
fn ordering_for(sort: Sort, filters: &Filters) -> Ordering {
    let by = |key: String, field: KeyField, direction: Direction| Ordering {
        key: Some(key),
        field: Some(field),
        direction,
    };

    match sort {
        Sort::Distance if filters.near.is_some() => by(
            distance_expression(),
            KeyField::Distance,
            Direction::Ascending,
        ),
        Sort::Rating => by(
            "c.quality_score".to_owned(),
            KeyField::Quality,
            Direction::Descending,
        ),
        // With no query every text term is zero and the blend is quality times
        // a constant — the same order as quality itself. Sorting by the bare
        // column rather than the expression is not a shortcut: an index cannot
        // serve `ORDER BY 0.5 * quality_score`, and this is the directory's
        // default page. Measured at 51,000 rows it is the difference between an
        // index scan and a sequential scan with a top-N sort.
        Sort::Best if filters.query.is_none() => by(
            "c.quality_score".to_owned(),
            KeyField::Quality,
            Direction::Descending,
        ),
        Sort::Best => by(rank_expression(), KeyField::Rank, Direction::Descending),
        Sort::Distance | Sort::Name => Ordering {
            key: None,
            field: None,
            direction: Direction::Ascending,
        },
    }
}

fn record_search(
    path: &'static str,
    filters: &Filters,
    sort: Option<Sort>,
    returned: usize,
    started: std::time::Instant,
) {
    tracing::debug!(
        path,
        sort = sort.map(|s| match s {
            Sort::Best => "best",
            Sort::Rating => "rating",
            Sort::Distance => "distance",
            Sort::Name => "name",
        }),
        has_query = filters.query.is_some(),
        routed_trades = filters.query_trade_ids.len(),
        trades = filters.trade_ids.len(),
        verified_only = filters.verified_only,
        has_postal_code = filters.postal_code.is_some(),
        has_near = filters.near.is_some(),
        radius_m = filters.near.map(|near| near.radius_m),
        has_bbox = filters.bbox.is_some(),
        returned,
        elapsed_ms = started.elapsed().as_millis(),
        "contractor search"
    );
}

pub async fn list(
    conn: &mut PgConnection,
    filters: &Filters,
    sort: Sort,
    limit: i64,
    cursor: Option<&Cursor>,
) -> Result<Page, AppError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let started = std::time::Instant::now();

    let ordering = ordering_for(sort, filters);
    let sql = list_sql(&ordering);

    let query = bind_filters(sqlx::query_as(&sql), filters);
    // Bound only when the statement refers to it; see `list_sql`.
    let query = match ordering.key {
        Some(_) => query.bind(cursor.and_then(|c| c.sort_key)),
        None => query,
    };

    let mut contractors = query
        .bind(cursor.map(|c| c.name.clone()))
        .bind(cursor.map(|c| c.id))
        .bind(limit + 1)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    record_search("list", filters, Some(sort), contractors.len(), started);

    // One extra row is fetched purely to answer "is there another page" without
    // a second count query.
    let next_cursor = if contractors.len() as i64 > limit {
        contractors.pop();
        contractors.last().map(|last| Cursor {
            name: last.display_name.clone(),
            id: last.id,
            // Whatever the ordering led with, read back off the row it stopped
            // at. Ordering and cursor are built from one expression, so the
            // value here is the same one the next page compares against.
            // Read from whichever column the ordering actually sorted by, so
            // the value the next page compares against is the value this page
            // stopped at.
            sort_key: match ordering.field {
                Some(KeyField::Rank) => last.rank_score,
                Some(KeyField::Quality) => last.quality_score,
                Some(KeyField::Distance) => last.distance_m,
                None => None,
            },
        })
    } else {
        None
    };

    Ok(Page {
        contractors,
        next_cursor,
    })
}

/// Map points: the same predicate, a narrower projection, a hard cap.
pub async fn map_points(
    conn: &mut PgConnection,
    filters: &Filters,
    limit: i64,
) -> Result<(Vec<PublicContractor>, bool), AppError> {
    let limit = limit.clamp(1, MAX_MAP_POINTS);
    let started = std::time::Instant::now();

    let sql = map_sql();

    let mut points = bind_filters(sqlx::query_as(&sql), filters)
        .bind(limit + 1)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    record_search("map", filters, None, points.len(), started);

    let truncated = points.len() as i64 > limit;
    if truncated {
        points.pop();
    }

    if truncated {
        // The cap is honest in the response, but a map that is *always*
        // truncated is a filter set nobody can narrow — worth seeing without
        // waiting for a report.
        tracing::debug!(limit, "map results were capped");
    }

    Ok((points, truncated))
}

pub async fn find_public(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<PublicContractor>, AppError> {
    let sql = find_sql("id");

    bind_filters(sqlx::query_as(&sql), &Filters::default())
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)
}

pub async fn find_public_by_slug(
    conn: &mut PgConnection,
    slug: &str,
) -> Result<Option<PublicContractor>, AppError> {
    let sql = find_sql("slug");

    bind_filters(sqlx::query_as(&sql), &Filters::default())
        .bind(slug)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for PublicContractor {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let precision: String = row.try_get("public_point_source")?;

        Ok(Self {
            id: row.try_get("id")?,
            slug: row.try_get("slug")?,
            display_name: row.try_get("display_name")?,
            verified: row.try_get("verified")?,
            verified_at: row.try_get("verified_at")?,
            bio: row.try_get("bio")?,
            website_url: row.try_get("website_url")?,
            public_phone: row.try_get("public_phone")?,
            postal_code: row.try_get("postal_code")?,
            accepts_dm: row.try_get("accepts_dm")?,
            is_claimed: row.try_get("is_claimed")?,
            lat: row.try_get("lat")?,
            lon: row.try_get("lon")?,
            location_precision: PublicPointSource::parse(&precision)
                .unwrap_or(PublicPointSource::None),
            license_no: row.try_get("license_no")?,
            license_status: row.try_get("license_status")?,
            address_line1: row.try_get("address_line1")?,
            address_city: row.try_get("address_city")?,
            address_state: row.try_get("address_state")?,
            trades: row.try_get("trades")?,
            distance_m: row.try_get("distance_m")?,
            google_rating: row.try_get("google_rating")?,
            google_review_count: row.try_get("google_review_count")?,
            google_place_url: row.try_get("google_place_url")?,
            address_is_owner_supplied: row.try_get("address_is_owner_supplied")?,
            license_address_line1: row.try_get("license_address_line1")?,
            license_address_city: row.try_get("license_address_city")?,
            google_review_url: row.try_get("google_review_url")?,
            yelp_url: row.try_get("yelp_url")?,
            // The raw object key, not yet a URL. `attach_photo_urls` rewrites
            // it once a caller with the store is in scope, the same way
            // `jobs::attach_photos` does — the row reader has no store and
            // should not grow one.
            photo_url: row.try_get("photo_storage_key")?,
            photo_width: row.try_get("photo_width")?,
            photo_height: row.try_get("photo_height")?,
            // Present only on the list query, which is the only one that
            // paginates and so the only one that needs a cursor key.
            rank_score: row.try_get("rank_score").ok().flatten(),
            quality_score: row.try_get("quality_score").ok().flatten(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near() -> Filters {
        Filters {
            near: Some(Near {
                lat: 34.0,
                lon: -118.0,
                radius_m: 25_000.0,
            }),
            ..Filters::default()
        }
    }

    fn text() -> Filters {
        Filters {
            query: Some("ibarra".to_owned()),
            ..Filters::default()
        }
    }

    /// Every statement this module can produce, so a new read path cannot be
    /// added without deciding what it does about the invariants below.
    fn every_statement() -> Vec<(&'static str, String)> {
        vec![
            (
                "list/best",
                list_sql(&ordering_for(Sort::Best, &Filters::default())),
            ),
            (
                "list/rating",
                list_sql(&ordering_for(Sort::Rating, &text())),
            ),
            ("list/name", list_sql(&ordering_for(Sort::Name, &text()))),
            (
                "list/distance",
                list_sql(&ordering_for(Sort::Distance, &near())),
            ),
            ("map", map_sql()),
            ("detail/id", find_sql("id")),
            ("detail/slug", find_sql("slug")),
        ]
    }

    /// The invariant the whole module exists to hold. `precise_point` is
    /// geocoded and never published: if distance search ran against it while
    /// the map published a centroid, the radius filter could be binary-searched
    /// to recover the address the centroid was protecting.
    #[test]
    fn no_statement_reads_the_precise_point() {
        for (name, sql) in every_statement() {
            assert!(
                !sql.contains("precise_point"),
                "{name} must never read precise_point: {sql}"
            );
            assert!(
                sql.contains("ST_Y(c.public_point::geometry)"),
                "{name} must publish the point the map shows: {sql}"
            );
        }
    }

    /// One shared WHERE clause is what stops the list and the map disagreeing
    /// about what matches. Detail runs it too, so a row the list excludes
    /// cannot be reached by guessing its slug.
    #[test]
    fn every_read_path_filters_through_the_shared_predicate() {
        for (name, sql) in every_statement() {
            assert!(
                sql.contains(PREDICATE),
                "{name} must filter through PREDICATE verbatim: {sql}"
            );
        }
    }

    /// The tail clauses number themselves from `PREDICATE_BINDS`. If the
    /// predicate grows a parameter and the constant is not moved with it, the
    /// cursor and the limit start reading filter values — silently, and only
    /// on the second page.
    #[test]
    fn the_predicate_declares_how_many_binds_it_uses() {
        // Both halves, so a bind that moves between them is still counted.
        let shared = format!("{SELECT} {FROM} {PREDICATE}");
        let highest = (1..=99)
            .filter(|n| shared.contains(&format!("${n}")))
            .max()
            .expect("the shared clauses bind something");

        assert_eq!(
            highest, PREDICATE_BINDS,
            "the shared clauses use ${highest} but PREDICATE_BINDS says \
             {PREDICATE_BINDS}; every tail clause is numbered from that constant"
        );

        let listing = list_sql(&ordering_for(Sort::Best, &Filters::default()));
        for offset in 1..=4 {
            assert!(
                listing.contains(&format!("${}", PREDICATE_BINDS + offset)),
                "the list tail should bind ${}",
                PREDICATE_BINDS + offset
            );
        }
        assert!(map_sql().contains(&format!("${}", PREDICATE_BINDS + 1)));
    }

    /// The ranking reads the same tsquery the predicate matched on, by
    /// referring to its bind slot. Renumbering the predicate without moving
    /// `QUERY_BIND` would rank by a radius.
    #[test]
    fn the_ranking_reads_the_query_bind() {
        let rank = rank_expression();

        assert!(rank.contains(&format!("${QUERY_BIND}")), "{rank}");
        assert!(
            PREDICATE.contains(&format!(
                "websearch_to_tsquery('public.english_unaccent', ${QUERY_BIND})"
            )),
            "the predicate and the ranking must read the same slot"
        );
    }

    /// A sort the caller cannot support degrades to the stable key rather than
    /// ordering by a column that is NULL for every row.
    #[test]
    fn a_sort_without_its_input_falls_back_to_the_stable_key() {
        let empty = Filters::default();

        assert!(ordering_for(Sort::Distance, &empty).key.is_none());
        assert_eq!(
            ordering_for(Sort::Distance, &empty).order_by(),
            "c.display_name, c.id"
        );
        assert_eq!(
            ordering_for(Sort::Name, &empty).order_by(),
            "c.display_name, c.id"
        );
    }

    /// Keyset pagination is only well-defined over a total order.
    #[test]
    fn every_ordering_ends_in_the_keyset_tuple() {
        for (sort, filters) in [
            (Sort::Best, Filters::default()),
            (Sort::Rating, Filters::default()),
            (Sort::Name, Filters::default()),
            (Sort::Distance, near()),
            (Sort::Distance, Filters::default()),
        ] {
            let order = ordering_for(sort, &filters).order_by();
            assert!(
                order.ends_with("c.display_name, c.id"),
                "{order} does not end in the cursor's tuple"
            );
        }
    }

    /// The defect this cursor rework exists to fix. Page two used to filter on
    /// `(display_name, id)` whatever the ORDER BY led with, so a distance or
    /// relevance sort silently dropped rows — and the front end stopped
    /// paginating those sorts rather than showing the wrong page.
    ///
    /// The comparison and the ordering are now built from one expression, and
    /// they have to point the same way: descending sorts take rows below where
    /// the page stopped, ascending ones above.
    #[test]
    fn the_cursor_compares_the_same_key_the_ordering_sorts_by() {
        for (sort, filters, expected) in [
            (Sort::Best, Filters::default(), "<"),
            (Sort::Rating, Filters::default(), "<"),
            (Sort::Distance, near(), ">"),
        ] {
            let ordering = ordering_for(sort, &filters);
            let key = ordering.key.clone().expect("a leading key");
            let sql = list_sql(&ordering);

            assert!(
                sql.contains(&format!("{key} {expected} $")),
                "{sort:?} must compare its own key with {expected}: {sql}"
            );
            assert!(
                sql.contains("AND (c.display_name, c.id) > ("),
                "{sort:?} must break ties on the ascending stable key: {sql}"
            );
            assert!(
                sql.contains(&format!("ORDER BY {key} {}", ordering.direction.sql())),
                "{sort:?} must order by the key it compares: {sql}"
            );
        }
    }

    /// With no query the ranking is quality alone, which is what turns browsing
    /// from alphabetical into best-first. With one, a text match always
    /// outweighs the quality term: somebody searching a business name wants
    /// that business, not the best-rated builder in the county.
    #[test]
    fn quality_orders_equals_and_never_overturns_a_text_match() {
        const _: () = assert!(
            QUALITY_WEIGHT < 1.0,
            "the match bonus is 1.0; a quality weight at or above it would let \
             an unmatched listing outrank a matched one"
        );

        let rank = rank_expression();
        assert!(rank.contains("c.quality_score"));
        assert!(rank.contains("ts_rank_cd"));
        assert!(
            rank.contains(&format!("${QUERY_BIND}::text IS NULL THEN 0.0")),
            "with no query the text terms must contribute nothing: {rank}"
        );
    }
}
