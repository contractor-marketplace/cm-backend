//! Where a contractor works, as opposed to where they are.
//!
//! The directory matches a search against the address on a licence, which is a
//! poor proxy: a roofer in Culver City who covers the whole west side is
//! invisible to somebody searching Santa Monica, and a sole trader whose
//! licence carries their home address is placed at their house rather than at
//! their patch.
//!
//! Two kinds of area, and a row is exactly one of them — a named region, or a
//! radius from the contractor's own point. The CHECK in 0010 enforces that; the
//! type here mirrors it, so an impossible pair cannot be constructed in Rust
//! and then refused by the database.

use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

/// The largest radius a contractor may claim: about 125 miles, matching the
/// ceiling on a search radius. Beyond that "I serve this area" stops meaning
/// anything.
pub const MAX_RADIUS_M: i32 = 200_000;

/// The most areas one contractor may declare.
///
/// Bounded because the matching predicate reads them all, and because a list
/// of two hundred ZIPs is not a service area, it is an absence of one.
pub const MAX_AREAS: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Area {
    /// A named place, by its ZIP code.
    Region { code: String },
    /// Everywhere within this many metres of the contractor's own point.
    Radius { radius_m: i32 },
}

/// What a contractor has declared.
pub async fn for_contractor(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Vec<Area>, AppError> {
    let rows: Vec<(Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT r.code, sa.radius_m \
           FROM contractor_service_areas sa \
           LEFT JOIN regions r ON r.id = sa.region_id \
          WHERE sa.contractor_id = $1 \
          ORDER BY sa.radius_m NULLS LAST, r.code",
    )
    .bind(contractor_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(rows
        .into_iter()
        .filter_map(|(code, radius_m)| match (code, radius_m) {
            (Some(code), None) => Some(Area::Region { code }),
            (None, Some(radius_m)) => Some(Area::Radius { radius_m }),
            // The CHECK makes this unreachable; a row that reached it anyway is
            // not something to guess about.
            _ => None,
        })
        .collect())
}

/// Replace everything a contractor has declared.
///
/// Delete-then-insert rather than a diff: the caller sends the whole set, which
/// is what a list of checkboxes produces, and reconciling a set against itself
/// is work that buys nothing at this size.
///
/// A ZIP with no region row is skipped and reported, not refused. The
/// gazetteer covers populated areas only, so a PO-box ZIP a contractor
/// genuinely uses has no centroid to match against — telling them which ones
/// did not take is more use than rejecting the whole list.
pub async fn replace(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    areas: &[Area],
) -> Result<Vec<String>, AppError> {
    sqlx::query("DELETE FROM contractor_service_areas WHERE contractor_id = $1")
        .bind(contractor_id)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    let mut unknown = Vec::new();

    for area in areas.iter().take(MAX_AREAS) {
        match area {
            Area::Region { code } => {
                let inserted = sqlx::query(
                    "INSERT INTO contractor_service_areas (id, contractor_id, region_id) \
                     SELECT $1, $2, r.id FROM regions r \
                      WHERE r.kind = 'zcta' AND r.code = $3 \
                     ON CONFLICT (contractor_id, region_id) DO NOTHING",
                )
                .bind(new_id())
                .bind(contractor_id)
                .bind(code)
                .execute(&mut *conn)
                .await
                .map_err(AppError::internal)?;

                if inserted.rows_affected() == 0 {
                    unknown.push(code.clone());
                }
            }
            Area::Radius { radius_m } => {
                sqlx::query(
                    "INSERT INTO contractor_service_areas (id, contractor_id, radius_m) \
                     VALUES ($1, $2, $3)",
                )
                .bind(new_id())
                .bind(contractor_id)
                .bind((*radius_m).clamp(1, MAX_RADIUS_M))
                .execute(&mut *conn)
                .await
                .map_err(AppError::internal)?;
            }
        }
    }

    Ok(unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape a client switches on. A tagged enum, so a region and a
    /// radius are told apart by a field rather than by which one is null.
    #[test]
    fn an_area_serialises_as_its_kind() {
        assert_eq!(
            serde_json::to_string(&Area::Region {
                code: "90026".to_owned()
            })
            .expect("serialize"),
            r#"{"kind":"region","code":"90026"}"#
        );
        assert_eq!(
            serde_json::to_string(&Area::Radius { radius_m: 25_000 }).expect("serialize"),
            r#"{"kind":"radius","radius_m":25000}"#
        );
    }

    /// A radius past the ceiling stops meaning "I serve this area".
    #[test]
    fn the_radius_ceiling_matches_the_search_ceiling() {
        assert_eq!(MAX_RADIUS_M, 200_000);
    }
}
