//! Homeowner profiles.
//!
//! Optional by design: an account with no profile is mid-onboarding, not
//! broken. Role is implied by which profile exists rather than by a column on
//! `users`, so a person can be a homeowner and later also claim a listing.

use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize)]
pub struct HomeownerProfile {
    pub user_id: Uuid,
    pub display_name: String,
    pub postal_code: Option<String>,
    pub contact_phone: Option<String>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for HomeownerProfile {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            user_id: row.try_get("user_id")?,
            display_name: row.try_get("display_name")?,
            postal_code: row.try_get("postal_code")?,
            contact_phone: row.try_get("contact_phone")?,
        })
    }
}

pub async fn find(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<Option<HomeownerProfile>, AppError> {
    sqlx::query_as(
        "SELECT user_id, display_name, postal_code, contact_phone \
           FROM homeowner_profiles WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn upsert(
    conn: &mut PgConnection,
    user_id: Uuid,
    display_name: &str,
    postal_code: Option<&str>,
    contact_phone: Option<&str>,
    region_id: Option<Uuid>,
) -> Result<HomeownerProfile, AppError> {
    sqlx::query_as(
        "INSERT INTO homeowner_profiles \
             (user_id, display_name, postal_code, contact_phone, region_id) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (user_id) DO UPDATE SET \
             display_name = EXCLUDED.display_name, \
             postal_code = EXCLUDED.postal_code, \
             contact_phone = EXCLUDED.contact_phone, \
             region_id = EXCLUDED.region_id, \
             updated_at = now() \
         RETURNING user_id, display_name, postal_code, contact_phone",
    )
    .bind(user_id)
    .bind(display_name)
    .bind(postal_code)
    .bind(contact_phone)
    .bind(region_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)
}
