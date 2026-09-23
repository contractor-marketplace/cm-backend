//! Work photos on a claimed listing.
//!
//! The same shape as `job_photos`, for the same reasons: this table is the
//! index, the bytes are in object storage, and the domain layer removes the two
//! together rather than trusting the foreign key to mean anything about an
//! object. See that module's header.

use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

/// How many work photos one listing may carry.
///
/// Enforced here rather than in the schema: a CHECK cannot count rows in its own
/// table. Twelve shows a range of work without turning the profile into a
/// gallery that outweighs the licence, which is still the point of the page.
pub const MAX_PER_CONTRACTOR: i64 = 12;

pub struct NewPhoto<'a> {
    pub id: Uuid,
    pub contractor_id: Uuid,
    pub storage_key: &'a str,
    pub byte_size: i64,
    pub width: i32,
    pub height: i32,
}

/// A stored photo, as the repository sees it. The URL is built by the caller
/// from `storage_key`, so the bucket can move without touching data.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PhotoRow {
    pub id: Uuid,
    pub contractor_id: Uuid,
    pub storage_key: String,
    pub width: i32,
    pub height: i32,
}

/// A stored photo, as published.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Photo {
    pub id: Uuid,
    pub url: String,
    pub width: i32,
    pub height: i32,
}

/// Insert at the next free position.
///
/// The position is computed in the statement rather than read-then-written, so
/// two uploads racing on one listing cannot both pick the same slot.
pub async fn insert(conn: &mut PgConnection, photo: NewPhoto<'_>) -> Result<PhotoRow, AppError> {
    sqlx::query_as(
        "INSERT INTO contractor_photos \
             (id, contractor_id, storage_key, byte_size, width, height, position) \
         SELECT $1, $2, $3, $4, $5, $6, \
                COALESCE(MAX(position) + 1, 0) FROM contractor_photos WHERE contractor_id = $2 \
         RETURNING id, contractor_id, storage_key, width, height",
    )
    .bind(photo.id)
    .bind(photo.contractor_id)
    .bind(photo.storage_key)
    .bind(photo.byte_size)
    .bind(photo.width)
    .bind(photo.height)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn count_for_contractor(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<i64, AppError> {
    sqlx::query_scalar("SELECT count(*) FROM contractor_photos WHERE contractor_id = $1")
        .bind(contractor_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// Every photo on a listing, in display order.
pub async fn for_contractor(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Vec<PhotoRow>, AppError> {
    sqlx::query_as(
        "SELECT id, contractor_id, storage_key, width, height \
           FROM contractor_photos WHERE contractor_id = $1 \
          ORDER BY position",
    )
    .bind(contractor_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Delete one photo, returning its storage key so the caller can remove the
/// object. `None` means it was not there, or not on that listing.
pub async fn delete(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    photo_id: Uuid,
) -> Result<Option<String>, AppError> {
    sqlx::query_scalar(
        "DELETE FROM contractor_photos WHERE id = $1 AND contractor_id = $2 RETURNING storage_key",
    )
    .bind(photo_id)
    .bind(contractor_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}
