//! Photo rows for jobs.
//!
//! This table is the index; the bytes are in object storage. The row is the
//! authority on whether a photo is still visible, and the object is the thing
//! that has to be deleted for that to mean anything — which is why the domain
//! layer removes the object and the row together rather than relying on the
//! foreign key alone.

use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

/// How many photos one job may carry.
///
/// Enforced here rather than in the schema: a CHECK cannot count rows in its own
/// table. Eight is enough to show a room from every angle and few enough that a
/// board page stays light.
pub const MAX_PER_JOB: i64 = 8;

pub struct NewPhoto<'a> {
    pub id: Uuid,
    pub job_id: Uuid,
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
    pub job_id: Uuid,
    pub storage_key: String,
    pub width: i32,
    pub height: i32,
}

/// Insert at the next free position.
///
/// The position is computed in the statement rather than read-then-written, so
/// two uploads racing on one job cannot both pick the same slot: the unique
/// constraint would reject the loser, and it does not get the chance to be
/// wrong in the first place.
pub async fn insert(conn: &mut PgConnection, photo: NewPhoto<'_>) -> Result<PhotoRow, AppError> {
    sqlx::query_as(
        "INSERT INTO job_photos (id, job_id, storage_key, byte_size, width, height, position) \
         SELECT $1, $2, $3, $4, $5, $6, \
                COALESCE(MAX(position) + 1, 0) FROM job_photos WHERE job_id = $2 \
         RETURNING id, job_id, storage_key, width, height",
    )
    .bind(photo.id)
    .bind(photo.job_id)
    .bind(photo.storage_key)
    .bind(photo.byte_size)
    .bind(photo.width)
    .bind(photo.height)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn count_for_job(conn: &mut PgConnection, job_id: Uuid) -> Result<i64, AppError> {
    sqlx::query_scalar("SELECT count(*) FROM job_photos WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// Every photo for a set of jobs, in display order.
///
/// Taken as a second query rather than joined into the board projection. A join
/// would multiply each job row by its photo count, and the keyset cursor counts
/// rows — page two would start in the middle of a job.
pub async fn for_jobs(
    conn: &mut PgConnection,
    job_ids: &[Uuid],
) -> Result<Vec<PhotoRow>, AppError> {
    if job_ids.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query_as(
        "SELECT id, job_id, storage_key, width, height \
           FROM job_photos WHERE job_id = ANY($1) \
          ORDER BY job_id, position",
    )
    .bind(job_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Delete one photo, returning its storage key so the caller can remove the
/// object. `None` means it was not there, or not on that job.
pub async fn delete(
    conn: &mut PgConnection,
    job_id: Uuid,
    photo_id: Uuid,
) -> Result<Option<String>, AppError> {
    sqlx::query_scalar(
        "DELETE FROM job_photos WHERE id = $1 AND job_id = $2 RETURNING storage_key",
    )
    .bind(photo_id)
    .bind(job_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Every storage key for a job, for deleting the objects when it is withdrawn.
pub async fn keys_for_job(conn: &mut PgConnection, job_id: Uuid) -> Result<Vec<String>, AppError> {
    sqlx::query_scalar("SELECT storage_key FROM job_photos WHERE job_id = $1")
        .bind(job_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// Drop every row for a job. The objects are the caller's problem, and it must
/// read `keys_for_job` first — after this the keys are gone.
pub async fn delete_all_for_job(conn: &mut PgConnection, job_id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM job_photos WHERE job_id = $1")
        .bind(job_id)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;
    Ok(())
}
