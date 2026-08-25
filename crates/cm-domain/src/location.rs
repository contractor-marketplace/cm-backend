//! Where a contractor appears on the map.
//!
//! One function decides this for the whole system — `republish`. The rule:
//!
//! * A listing whose address visibility is `public` and which has been geocoded
//!   publishes its exact point. That is the default, because every listing here
//!   comes from the CSLB public licence register, which publishes the business
//!   address of each licensee as a matter of law. A ZIP centroid concealed
//!   nothing anyone could not look up in seconds; it only made the directory
//!   worse at finding somebody nearby.
//! * A listing marked `protected`, or one the geocoder could not place,
//!   publishes its ZIP-code centroid.
//! * One with neither is unlocated, and is absent from distance search rather
//!   than appearing at a guessed position.
//!
//! Search reads the published point and never the precise one. That is
//! unchanged by publishing exact addresses, and it is what stops a radius filter
//! being binary-searched to recover a point the map was rounding off. For a
//! `protected` listing the guarantee still bites; for a public one it holds
//! trivially, because there is nothing finer behind the published point.
//!
//! Jobs work the other way round and share none of this: `jobs` has no address
//! column at all. A homeowner's address was never public, so it is not ours to
//! publish. See the header of `migrations/0017_jobs.sql`.

use cm_core::AppError;
use cm_db::repo::contractors::{self, AddressVisibility, PublicPointSource};
use cm_db::repo::reference;
use sqlx::PgConnection;
use uuid::Uuid;

/// Recompute the published point from stored state.
///
/// Safe to call at any time and as often as you like — it reads what is already
/// on the row rather than taking a point from the caller, so it cannot lose
/// one. That property is the fix for a real bug: this used to be
/// `apply_zip_centroid`, which called `set_location` with `precise: None`, and
/// `set_location` writes NULL for a `None` rather than leaving the column
/// alone. Since the importer calls this for every row, re-importing the CSLB
/// export erased every geocoded point in the table. That was survivable while
/// the published point was a centroid anyway. It is not survivable now that the
/// precise point IS the published point.
pub async fn republish(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<PublicPointSource, AppError> {
    let Some(inputs) = contractors::location_inputs(conn, contractor_id).await? else {
        return Ok(PublicPointSource::None);
    };

    if inputs.address_visibility == AddressVisibility::Public {
        if let Some((lon, lat)) = inputs.precise_point {
            contractors::set_location(
                conn,
                contractor_id,
                Some((lon, lat)),
                Some((lon, lat)),
                PublicPointSource::Exact,
            )
            .await?;
            return Ok(PublicPointSource::Exact);
        }
    }

    // Either the listing asked to be kept off the map, or we have not managed to
    // geocode it. Both land on the centroid.
    let centroid = match &inputs.postal_code {
        Some(code) => reference::find_zcta(conn, code).await?,
        None => None,
    };

    match centroid {
        Some(region) => {
            contractors::set_location(
                conn,
                contractor_id,
                inputs.precise_point,
                Some((region.lon, region.lat)),
                PublicPointSource::ZipCentroid,
            )
            .await?;
            Ok(PublicPointSource::ZipCentroid)
        }
        // No centroid known for this ZIP: leave the contractor unlocated rather
        // than dropping a pin somewhere plausible.
        None => {
            contractors::set_location(
                conn,
                contractor_id,
                inputs.precise_point,
                None,
                PublicPointSource::None,
            )
            .await?;
            Ok(PublicPointSource::None)
        }
    }
}

/// Store a geocoding result, then publish whatever the rule allows.
///
/// The write and the decision are separate steps on purpose: the precise point
/// is worth keeping even for a listing that is not publishing it, because the
/// answer changes the moment visibility does, and re-geocoding forty thousand
/// addresses to recover something we already had would be absurd.
pub async fn apply_geocode(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    lat: f64,
    lon: f64,
) -> Result<PublicPointSource, AppError> {
    if contractors::location_inputs(conn, contractor_id)
        .await?
        .is_none()
    {
        return Ok(PublicPointSource::None);
    }

    // Written first so `republish` can read it back, which keeps one function
    // in charge of the publish decision rather than two that must agree.
    contractors::set_precise_point(conn, contractor_id, lon, lat).await?;
    republish(conn, contractor_id).await
}

/// Re-apply the rule after a visibility change, so turning publication off takes
/// effect immediately rather than at the next geocode.
pub async fn reapply(conn: &mut PgConnection, contractor_id: Uuid) -> Result<(), AppError> {
    republish(conn, contractor_id).await?;
    Ok(())
}
