//! Where a contractor appears on the map — and where it must not.
//!
//! One function decides this for the whole system. The rule:
//!
//! * A listing whose address visibility is `public` — only ever a *claimed*
//!   listing, enforced by a CHECK constraint — publishes its exact point.
//! * Everything else publishes its ZIP-code centroid.
//! * A contractor with neither is unlocated, and is absent from distance search
//!   rather than appearing at a guessed position.
//!
//! Search reads the same published point, so a protected address cannot be
//! recovered by binary-searching the radius filter.

use cm_core::AppError;
use cm_db::repo::contractors::{self, AddressVisibility, PublicPointSource};
use cm_db::repo::reference;
use sqlx::PgConnection;
use uuid::Uuid;

/// Recompute the published point from the ZIP centroid, leaving any precise
/// point alone.
pub async fn apply_zip_centroid(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<(), AppError> {
    let Some(inputs) = contractors::location_inputs(conn, contractor_id).await? else {
        return Ok(());
    };

    let centroid = match &inputs.postal_code {
        Some(code) => reference::find_zcta(conn, code).await?,
        None => None,
    };

    match centroid {
        Some(region) => {
            contractors::set_location(
                conn,
                contractor_id,
                None,
                Some((region.lon, region.lat)),
                PublicPointSource::ZipCentroid,
            )
            .await
        }
        // No centroid known for this ZIP: leave the contractor unlocated rather
        // than dropping a pin somewhere plausible.
        None => Ok(()),
    }
}

/// Store a geocoding result, publishing only what the visibility rule allows.
pub async fn apply_geocode(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    lat: f64,
    lon: f64,
) -> Result<PublicPointSource, AppError> {
    let Some(inputs) = contractors::location_inputs(conn, contractor_id).await? else {
        return Ok(PublicPointSource::None);
    };

    let publish_exact = inputs.address_visibility == AddressVisibility::Public && inputs.is_claimed;

    if publish_exact {
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

    // The precise point is stored — it is needed if the owner later chooses to
    // publish it — but the published point stays the centroid. No API read path
    // selects the precise column.
    let centroid = match &inputs.postal_code {
        Some(code) => reference::find_zcta(conn, code).await?,
        None => None,
    };

    match centroid {
        Some(region) => {
            contractors::set_location(
                conn,
                contractor_id,
                Some((lon, lat)),
                Some((region.lon, region.lat)),
                PublicPointSource::ZipCentroid,
            )
            .await?;
            Ok(PublicPointSource::ZipCentroid)
        }
        None => {
            contractors::set_location(
                conn,
                contractor_id,
                Some((lon, lat)),
                None,
                PublicPointSource::None,
            )
            .await?;
            Ok(PublicPointSource::None)
        }
    }
}

/// Re-apply the rule after a visibility change, so turning publication off
/// takes effect immediately rather than at the next geocode.
pub async fn reapply(conn: &mut PgConnection, contractor_id: Uuid) -> Result<(), AppError> {
    let Some(inputs) = contractors::location_inputs(conn, contractor_id).await? else {
        return Ok(());
    };

    if inputs.address_visibility == AddressVisibility::Protected {
        apply_zip_centroid(conn, contractor_id).await?;
    }

    Ok(())
}
