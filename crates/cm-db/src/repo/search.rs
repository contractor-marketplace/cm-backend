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
}

/// The hard ceiling on a page, whatever the caller asks for.
pub const MAX_PAGE: i64 = 50;
/// The hard ceiling on map points. A zoomed-out viewport degrades honestly
/// rather than returning a silently partial map.
pub const MAX_MAP_POINTS: i64 = 500;

/// One shared WHERE clause, so list and map can never disagree about what
/// matches — a map showing pins the list omits is a bug report nobody can
/// reproduce.
const PREDICATE: &str = "\
    ($1::float8 IS NULL OR ST_DWithin(c.public_point, \
        ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)) \
    AND ($4::text IS NULL OR c.search_doc @@ websearch_to_tsquery('public.english_unaccent', $4) \
         OR c.display_name % $4) \
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

pub async fn list(
    conn: &mut PgConnection,
    filters: &Filters,
    sort: Sort,
    limit: i64,
    cursor: Option<&Cursor>,
) -> Result<Page, AppError> {
    let limit = limit.clamp(1, MAX_PAGE);

    // Keyset pagination is only well-defined over a total order, so every sort
    // ends in (display_name, id) and the cursor is that pair. Distance and
    // relevance order the first page; ties and subsequent pages fall back to
    // the stable key rather than drifting.
    let order = match sort {
        Sort::Distance if filters.near.is_some() => "distance_m ASC NULLS LAST, display_name, id",
        Sort::Relevance if filters.query.is_some() => {
            "ts_rank(c.search_doc, websearch_to_tsquery('public.english_unaccent', $4)) DESC, \
             display_name, id"
        }
        _ => "display_name, id",
    };

    let sql = format!(
        "{SELECT} WHERE {PREDICATE} \
           AND ($12::text IS NULL OR (c.display_name, c.id) > ($12, $13::uuid)) \
         ORDER BY {order} LIMIT $14"
    );

    let mut contractors = bind_filters(sqlx::query_as(&sql), filters)
        .bind(cursor.map(|c| c.name.clone()))
        .bind(cursor.map(|c| c.id))
        .bind(limit + 1)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)?;

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

    let sql = format!(
        "{SELECT} WHERE {PREDICATE} AND c.public_point IS NOT NULL \
         ORDER BY c.verified DESC, c.display_name, c.id LIMIT $12"
    );

    let mut points = bind_filters(sqlx::query_as(&sql), filters)
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

pub async fn find_public(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<PublicContractor>, AppError> {
    let sql = format!("{SELECT} WHERE {PREDICATE} AND c.id = $12");

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
    let sql = format!("{SELECT} WHERE {PREDICATE} AND c.slug = $12");

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
