//! Where a contractor works, as opposed to where they are.
//!
//! The directory matches a search against the address on a licence, which is a
//! poor proxy: a roofer in Culver City who covers the whole west side is
//! invisible to somebody searching Santa Monica, and a sole trader whose
//! licence carries their home address is placed at their house rather than at
//! their patch.
//!
//! Two kinds of area, stored differently because they are differently shaped.
//! The travel radius is `contractors.service_radius_m` — one value, always
//! present, defaulting to 25 miles for every listing including the unclaimed
//! majority. Named regions are rows in `contractor_service_areas` — zero or
//! more, and only ever set by a claimant.
//!
//! The wire type keeps them together as one tagged enum, because to the person
//! filling the form they are two answers to one question. 0030 moved the
//! storage; this module is where the two shapes are reconciled.

use cm_core::{new_id, AppError};
use sqlx::PgConnection;

use super::search::DEFAULT_SERVICE_RADIUS_M;
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
///
/// The radius always comes back, because every contractor has one — a listing
/// nobody has claimed still covers 25 miles. The editor therefore opens showing
/// the radius in force rather than an empty control that implies "nowhere".
pub async fn for_contractor(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Vec<Area>, AppError> {
    let radius_m: Option<i32> =
        sqlx::query_scalar("SELECT service_radius_m FROM contractors WHERE id = $1")
            .bind(contractor_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(AppError::internal)?;

    let codes: Vec<String> = sqlx::query_scalar(
        "SELECT r.code \
           FROM contractor_service_areas sa \
           JOIN regions r ON r.id = sa.region_id \
          WHERE sa.contractor_id = $1 \
          ORDER BY r.code",
    )
    .bind(contractor_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    // Radius first: it is the one every listing has, and the form leads with it.
    Ok(radius_m
        .map(|radius_m| Area::Radius { radius_m })
        .into_iter()
        .chain(codes.into_iter().map(|code| Area::Region { code }))
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

    // A radius is never absent, only unstated. Sending no radius means "leave
    // it at the default" rather than "I travel nowhere", so the column is reset
    // rather than cleared — there is no such thing as a contractor with no
    // coverage, and a form that could produce one would be a way to vanish from
    // the directory by accident.
    let radius = areas
        .iter()
        .find_map(|area| match area {
            Area::Radius { radius_m } => Some(*radius_m),
            Area::Region { .. } => None,
        })
        .unwrap_or(DEFAULT_SERVICE_RADIUS_M)
        .clamp(1, MAX_RADIUS_M);

    sqlx::query("UPDATE contractors SET service_radius_m = $2 WHERE id = $1")
        .bind(contractor_id)
        .bind(radius)
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
            // Already applied above, to the contractor rather than to this
            // table. Handled there because there is exactly one of them and it
            // has to be written even when the caller sends none.
            Area::Radius { .. } => {}
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
