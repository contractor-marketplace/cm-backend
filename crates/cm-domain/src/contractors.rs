//! A claimant's own listing: the profile photo, their work photos, and
//! re-locating after an edit.
//!
//! The rest of the profile edit is a single column write and stays in the
//! handler. These two are here because they are not: a photo touches the object
//! store as well as the database and must not leave an orphan in either
//! direction, and an address change has to move the map pin, which is a
//! decision about published location rather than a field update.

use chrono::Utc;
use cm_auth::ratelimit;
use cm_core::{new_id, AppError, Secret};
use cm_db::repo::audit::{self, ActorKind, AuditEvent};
use cm_db::repo::{contractor_photos, contractors, geocode};
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

/// Work photos are bucketed like job photos rather than like the profile
/// photo: a listing carries up to twelve, and re-doing a set after a bad batch
/// should not lock the owner out for a day.
fn work_photo_upload_policy() -> ratelimit::Policy {
    ratelimit::Policy {
        name: "contractor_work_photo:user",
        limit: 60,
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

/// Add a photograph of the claimant's work.
///
/// The same order as `jobs::attach_photo`: the file is normalised before
/// anything is written, so a bad upload costs nothing; the object is written
/// before the row, so a failure leaves an orphaned object rather than a row
/// pointing at nothing.
pub async fn add_work_photo(
    pool: &PgPool,
    store: &Store,
    pepper: &Secret<String>,
    user_id: Uuid,
    contractor_id: Uuid,
    bytes: &[u8],
    request_id: Option<String>,
) -> Result<contractor_photos::Photo, AppError> {
    ratelimit::enforce(
        pool,
        pepper,
        work_photo_upload_policy(),
        &user_id.to_string(),
        Utc::now(),
    )
    .await?;

    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    owned_listing(&mut conn, user_id, contractor_id).await?;

    let existing = contractor_photos::count_for_contractor(&mut conn, contractor_id).await?;
    if existing >= contractor_photos::MAX_PER_CONTRACTOR {
        return Err(AppError::invalid(format!(
            "A listing can have up to {} work photos.",
            contractor_photos::MAX_PER_CONTRACTOR
        )));
    }

    // A photograph of a finished kitchen carries the client's coordinates in
    // its EXIF. Re-encoding discards that by construction.
    let normalised = cm_storage::normalise(bytes)?;

    let id = new_id();
    let key = cm_storage::contractor_photo_key(contractor_id, id);
    let url = store.put(&key, &normalised).await?;

    let row = contractor_photos::insert(
        &mut conn,
        contractor_photos::NewPhoto {
            id,
            contractor_id,
            storage_key: &key,
            byte_size: normalised.bytes.len() as i64,
            width: normalised.width as i32,
            height: normalised.height as i32,
        },
    )
    .await
    .inspect_err(|_| {
        tracing::error!(%key, %contractor_id, "a work photo object was stored but its row was not");
    })?;

    audit::record(
        &mut conn,
        AuditEvent::new("contractor.work_photo_added", "contractor_photos")
            .actor(ActorKind::User, Some(user_id))
            .subject(id)
            .data(serde_json::json!({
                "contractor_id": contractor_id,
                "bytes": normalised.bytes.len(),
            }))
            .request_id(request_id),
    )
    .await?;

    Ok(contractor_photos::Photo {
        id: row.id,
        url,
        width: row.width,
        height: row.height,
    })
}

/// Remove a work photo. The row goes first, then the object: the owner asked
/// for it gone, and it is gone from the page whether or not storage agrees.
pub async fn remove_work_photo(
    pool: &PgPool,
    store: &Store,
    user_id: Uuid,
    contractor_id: Uuid,
    photo_id: Uuid,
    request_id: Option<String>,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await.map_err(AppError::internal)?;
    owned_listing(&mut tx, user_id, contractor_id).await?;

    let key = contractor_photos::delete(&mut tx, contractor_id, photo_id)
        .await?
        .ok_or(AppError::NotFound)?;

    audit::record(
        &mut tx,
        AuditEvent::new("contractor.work_photo_removed", "contractor_photos")
            .actor(ActorKind::User, Some(user_id))
            .subject(photo_id)
            .data(serde_json::json!({ "contractor_id": contractor_id }))
            .request_id(request_id),
    )
    .await?;

    tx.commit().await.map_err(AppError::internal)?;

    if let Err(error) = store.delete(&key).await {
        tracing::error!(%key, %photo_id, ?error, "a removed work photo left an orphaned object");
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
