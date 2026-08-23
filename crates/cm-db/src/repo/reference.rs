//! Reference data: trades and regions.

use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Trade {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub cslb_classification: Option<String>,
}

/// The canonical trade set for the launch county.
///
/// Seeded rather than imported: the CSLB classification list is long, and v1
/// searches on the handful of trades the product actually offers as filters.
/// Unrecognised classifications on a licence are simply not mapped.
pub const CANONICAL_TRADES: &[(&str, &str, &str)] = &[
    ("general-contractor", "General Contractor", "B"),
    ("electrician", "Electrician", "C-10"),
    ("plumber", "Plumber", "C-36"),
    ("roofer", "Roofer", "C-39"),
    ("painter", "Painter", "C-33"),
    ("landscaper", "Landscaper", "C-27"),
];

/// Insert any canonical trade that is missing. Idempotent.
pub async fn seed_trades(conn: &mut PgConnection) -> Result<u64, AppError> {
    let mut inserted = 0;
    for (order, (slug, name, classification)) in CANONICAL_TRADES.iter().enumerate() {
        let result = sqlx::query(
            "INSERT INTO trades (id, slug, name, cslb_classification, sort_order) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (slug) DO NOTHING",
        )
        .bind(new_id())
        .bind(slug)
        .bind(name)
        .bind(classification)
        .bind(order as i32)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;
        inserted += result.rows_affected();
    }

    Ok(inserted)
}

pub async fn all_trades(conn: &mut PgConnection) -> Result<Vec<Trade>, AppError> {
    sqlx::query_as(
        "SELECT id, slug, name, cslb_classification FROM trades \
          WHERE active ORDER BY sort_order, name",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Trade {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            slug: row.try_get("slug")?,
            name: row.try_get("name")?,
            cslb_classification: row.try_get("cslb_classification")?,
        })
    }
}

/// Trade ids for a set of slugs, for filtering.
pub async fn trade_ids_for_slugs(
    conn: &mut PgConnection,
    slugs: &[String],
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query_scalar("SELECT id FROM trades WHERE slug = ANY($1)")
        .bind(slugs)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// One ZIP-code area: its centroid is the published point for every contractor
/// whose exact address is protected.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Region {
    pub id: Uuid,
    pub kind: String,
    pub code: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

pub async fn upsert_zcta(
    conn: &mut PgConnection,
    code: &str,
    name: &str,
    lat: f64,
    lon: f64,
    source: &str,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "INSERT INTO regions (id, kind, code, name, centroid, source) \
         VALUES ($1, 'zcta', $2, $3, ST_SetSRID(ST_MakePoint($4, $5), 4326)::geography, $6) \
         ON CONFLICT (kind, code) DO UPDATE \
             SET name = EXCLUDED.name, centroid = EXCLUDED.centroid, \
                 source = EXCLUDED.source, updated_at = now()",
    )
    .bind(new_id())
    .bind(code)
    .bind(name)
    .bind(lon)
    .bind(lat)
    .bind(source)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

pub async fn find_zcta(conn: &mut PgConnection, code: &str) -> Result<Option<Region>, AppError> {
    // Never selects the geography itself: sqlx cannot decode PostGIS types, and
    // a query that returns one fails at the boundary.
    sqlx::query_as(
        "SELECT id, kind, code, name, ST_Y(centroid::geometry) AS lat, \
                ST_X(centroid::geometry) AS lon \
           FROM regions WHERE kind = 'zcta' AND code = $1",
    )
    .bind(code)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn list_zctas(conn: &mut PgConnection) -> Result<Vec<Region>, AppError> {
    sqlx::query_as(
        "SELECT id, kind, code, name, ST_Y(centroid::geometry) AS lat, \
                ST_X(centroid::geometry) AS lon \
           FROM regions WHERE kind = 'zcta' ORDER BY code",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Region {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            kind: row.try_get("kind")?,
            code: row.try_get("code")?,
            name: row.try_get("name")?,
            lat: row.try_get("lat")?,
            lon: row.try_get("lon")?,
        })
    }
}
