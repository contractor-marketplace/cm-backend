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
    Relevance,
    Distance,
    Name,
}

/// Where the previous page ended. Encoded opaquely at the edge.
#[derive(Debug, Clone)]
pub struct Cursor {
    pub name: String,
    pub id: Uuid,
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

/// One shared WHERE clause, so list and map can never disagree about what
/// matches — a map showing pins the list omits is a bug report nobody can
/// reproduce.
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
           c.google_place_url \
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

/// The list statement. Tail binds: cursor name, cursor id, limit.
fn list_sql(order: &str) -> String {
    let (name, id, limit) = (
        PREDICATE_BINDS + 1,
        PREDICATE_BINDS + 2,
        PREDICATE_BINDS + 3,
    );
    format!(
        "{SELECT} WHERE {PREDICATE} \
           AND (${name}::text IS NULL OR (c.display_name, c.id) > (${name}, ${id}::uuid)) \
         ORDER BY {order} LIMIT ${limit}"
    )
}

/// The map statement: the same predicate, a narrower ordering, one tail bind.
fn map_sql() -> String {
    let limit = PREDICATE_BINDS + 1;
    format!(
        "{SELECT} WHERE {PREDICATE} AND c.public_point IS NOT NULL \
         ORDER BY c.verified DESC, c.display_name, c.id LIMIT ${limit}"
    )
}

/// The detail statement, narrowed by `c.id` or `c.slug`.
///
/// It runs the full predicate against a default `Filters`, so detail and list
/// agree about what a visitor may see: a row the list would exclude is not
/// reachable by guessing its slug.
fn find_sql(column: &str) -> String {
    let value = PREDICATE_BINDS + 1;
    format!("{SELECT} WHERE {PREDICATE} AND c.{column} = ${value}")
}

/// The ordering for a sort, given what the caller actually supplied.
///
/// Keyset pagination is only well-defined over a total order, so every sort
/// ends in (display_name, id) and the cursor is that pair. Distance and
/// relevance order the first page; ties and subsequent pages fall back to the
/// stable key rather than drifting.
fn order_for(sort: Sort, filters: &Filters) -> String {
    match sort {
        Sort::Distance if filters.near.is_some() => {
            "distance_m ASC NULLS LAST, display_name, id".to_owned()
        }
        Sort::Relevance if filters.query.is_some() => format!(
            "ts_rank(c.search_doc, \
             websearch_to_tsquery('public.english_unaccent', ${QUERY_BIND})) DESC, \
             display_name, id"
        ),
        _ => "display_name, id".to_owned(),
    }
}

/// What a search was shaped like, for the log.
///
/// The shape, never the text. `router.rs` deliberately keeps the query string
/// out of its HTTP spans because it carries caller-supplied values, and a
/// search term is exactly that — often a person's own business name. Knowing a
/// query *had* text is enough to explain a slow plan; knowing what somebody
/// typed is not ours to keep.
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
            Sort::Relevance => "relevance",
            Sort::Distance => "distance",
            Sort::Name => "name",
        }),
        has_query = filters.query.is_some(),
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

    let sql = list_sql(&order_for(sort, filters));

    let mut contractors = bind_filters(sqlx::query_as(&sql), filters)
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every statement this module can produce, so a new read path cannot be
    /// added without deciding what it does about the invariants below.
    fn every_statement() -> Vec<(&'static str, String)> {
        let near = Filters {
            near: Some(Near {
                lat: 34.0,
                lon: -118.0,
                radius_m: 25_000.0,
            }),
            ..Filters::default()
        };
        let text = Filters {
            query: Some("ibarra".to_owned()),
            ..Filters::default()
        };

        vec![
            (
                "list",
                list_sql(&order_for(Sort::Name, &Filters::default())),
            ),
            ("list/distance", list_sql(&order_for(Sort::Distance, &near))),
            (
                "list/relevance",
                list_sql(&order_for(Sort::Relevance, &text)),
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
    ///
    /// `cm-api/tests/directory.rs` proves this behaviourally against a real
    /// database, which is the stronger check because it would catch a leak
    /// through a join. This one names the mistake at the point of making it.
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
        let highest = (1..=99)
            .filter(|n| PREDICATE.contains(&format!("${n}")))
            .max()
            .expect("the predicate binds something");

        assert_eq!(
            highest, PREDICATE_BINDS,
            "PREDICATE uses ${highest} but PREDICATE_BINDS says {PREDICATE_BINDS}; \
             every tail clause is numbered from that constant"
        );

        // The tail binds land immediately after the predicate's, with no gap
        // and no collision.
        assert!(list_sql("display_name, id").contains(&format!("${}", PREDICATE_BINDS + 1)));
        assert!(list_sql("display_name, id").contains(&format!("${}", PREDICATE_BINDS + 3)));
        assert!(map_sql().contains(&format!("${}", PREDICATE_BINDS + 1)));
    }

    /// The relevance ordering ranks by the same tsquery the predicate matched
    /// on, by referring to its bind slot. Renumbering the predicate without
    /// moving `QUERY_BIND` would rank by a radius.
    #[test]
    fn the_relevance_ordering_reads_the_query_bind() {
        let filters = Filters {
            query: Some("ibarra".to_owned()),
            ..Filters::default()
        };
        let order = order_for(Sort::Relevance, &filters);

        assert!(order.contains(&format!("${QUERY_BIND}")), "{order}");
        assert!(
            PREDICATE.contains(&format!(
                "websearch_to_tsquery('public.english_unaccent', ${QUERY_BIND})"
            )),
            "the predicate and the ordering must read the same slot"
        );
    }

    /// A sort the caller cannot support degrades to the stable key rather than
    /// ordering by a column that is NULL for every row.
    #[test]
    fn a_sort_without_its_input_falls_back_to_the_stable_key() {
        let empty = Filters::default();

        assert_eq!(order_for(Sort::Distance, &empty), "display_name, id");
        assert_eq!(order_for(Sort::Relevance, &empty), "display_name, id");
        assert_eq!(order_for(Sort::Name, &empty), "display_name, id");
    }

    /// Keyset pagination is only well-defined over a total order.
    #[test]
    fn every_ordering_ends_in_the_keyset_tuple() {
        let near = Filters {
            near: Some(Near {
                lat: 34.0,
                lon: -118.0,
                radius_m: 1.0,
            }),
            ..Filters::default()
        };
        let text = Filters {
            query: Some("q".to_owned()),
            ..Filters::default()
        };

        for (sort, filters) in [
            (Sort::Name, &Filters::default()),
            (Sort::Distance, &near),
            (Sort::Relevance, &text),
        ] {
            let order = order_for(sort, filters);
            assert!(
                order.ends_with("display_name, id"),
                "{order} does not end in the cursor's tuple"
            );
        }
    }
}
