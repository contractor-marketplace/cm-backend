//! A claimant's own listing: the profile photo, and re-locating after an edit.
//!
//! The rest of the profile edit is a single column write and stays in the
//! handler. These two are here because they are not: a photo touches the object
//! store as well as the database and must not leave an orphan in either
//! direction, and an address change has to move the map pin, which is a
//! decision about published location rather than a field update.

use chrono::Utc;
use cm_auth::ratelimit;
use cm_core::{new_id, AppError, Secret};
use cm_db::repo::{contractors, geocode};
use cm_storage::Store;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// Deliberately tighter than the job-photo allowance of 100 a day. A listing
/// has one photo; a hundred uploads is somebody testing our storage bill.
fn photo_upload_policy() -> ratelimit::Policy {
    ratelimit::Policy {
        name: "contractor_photo:user",
        limit: 20,
        window: chrono::Duration::days(1),
    }
}

/// The listing this user owns, or `NotFound`.
///
/// A 404 rather than a 403, matching `jobs::attach_photo`: "that is not yours"
/// would confirm the id is real to somebody probing.
async fn owned_listing(
    conn: &mut PgConnection,
    user_id: Uuid,
    contractor_id: Uuid,
) -> Result<(), AppError> {
    match contractors::claimed_by(conn, user_id).await? {
        Some(owned) if owned == contractor_id => Ok(()),
        _ => Err(AppError::NotFound),
    }
}

/// The photo as the client gets it back.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProfilePhoto {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

/// Replace the listing's profile photo.
///
/// Ordered so that no failure can leave the row pointing at an object that does
/// not exist: the new object is written first, the row is repointed second, and
/// only then is the displaced object deleted. The worst case is a leaked
/// object, which costs storage; the alternative ordering's worst case is a
/// profile whose photo 404s, which costs the contractor their page.
pub async fn set_photo(
    pool: &PgPool,
    store: &Store,
    pepper: &Secret<String>,
    user_id: Uuid,
    contractor_id: Uuid,
    bytes: &[u8],
) -> Result<ProfilePhoto, AppError> {
    ratelimit::enforce(
        pool,
        pepper,
        photo_upload_policy(),
        &user_id.to_string(),
        Utc::now(),
    )
    .await?;

    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    owned_listing(&mut conn, user_id, contractor_id).await?;

    // The same normalising pass job photos take. A business photograph carries
    // the coordinates of the business in its EXIF, and re-encoding discards
    // that by construction rather than by remembering to strip a tag.
    let normalised = cm_storage::normalise(bytes)?;

    let key = cm_storage::contractor_photo_key(contractor_id, new_id());
    let url = store.put(&key, &normalised).await?;

    let displaced = contractors::set_photo(
        &mut conn,
        contractor_id,
        &key,
        normalised.width,
        normalised.height,
    )
    .await?;

    // Best effort. The row already points at the new object, so a failure here
    // leaks the old one rather than breaking the page — and reporting an error
    // now would tell the contractor their upload failed when it did not.
    if let Some(old) = displaced {
        let _ = store.delete(&old).await;
    }

    Ok(ProfilePhoto {
        url,
        width: normalised.width,
        height: normalised.height,
    })
}

/// Remove the listing's profile photo.
pub async fn remove_photo(
    pool: &PgPool,
    store: &Store,
    user_id: Uuid,
    contractor_id: Uuid,
) -> Result<(), AppError> {
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    owned_listing(&mut conn, user_id, contractor_id).await?;

    // Row first here, unlike the upload. "Delete my photo" has to take effect
    // on the page even if the object store is unreachable, and an object with
    // nothing pointing at it is invisible.
    if let Some(key) = contractors::clear_photo(&mut conn, contractor_id).await? {
        let _ = store.delete(&key).await;
    }

    Ok(())
}

/// Re-resolve a listing's pin after its address changed.
///
/// The published point comes from geocoding, so a contractor who corrects their
/// address would otherwise keep the old pin until the next CSLB import — the
/// page and the map disagreeing about where they are, which is the exact
/// failure the location invariants exist to prevent.
///
/// Enqueued rather than resolved inline: geocoding is a network call to a rate
/// limited third party, and it must not be able to make saving a profile slow
/// or fail. The worker picks it up within its poll interval.
pub async fn relocate_after_address_change(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<(), AppError> {
    let Some(address) = contractors::geocodable_address(conn, contractor_id).await? else {
        return Ok(());
    };

    geocode::enqueue(conn, contractor_id, &crate::import::address_hash(&address)).await?;
    Ok(())
}
