//! Federated identities.
//!
//! Every function here keys on `(provider, subject)`. There is deliberately no
//! lookup by email in this file, and a test asserts that no query in the crate
//! matches an identity that way: merging accounts on a shared address is the
//! takeover vector the whole design avoids.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Google,
    Facebook,
}

impl Provider {
    /// Matches the `oauth_identities.provider` CHECK. Adding a variant without
    /// a migration that widens that constraint fails at the insert, loudly.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Facebook => "facebook",
        }
    }

    /// For a value read back out of the database.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "google" => Some(Self::Google),
            "facebook" => Some(Self::Facebook),
            _ => None,
        }
    }

    /// How the provider names itself to a user, for messages they will read.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::Facebook => "Facebook",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OauthIdentity {
    pub id: Uuid,
    pub user_id: Uuid,
    pub provider: Provider,
    pub subject: String,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct IdentityRow {
    id: Uuid,
    user_id: Uuid,
    subject: String,
    created_at: DateTime<Utc>,
}

/// Resolve a provider identity to the account it belongs to.
pub async fn find_by_subject(
    conn: &mut PgConnection,
    provider: Provider,
    subject: &str,
) -> Result<Option<OauthIdentity>, AppError> {
    let row: Option<IdentityRow> = sqlx::query_as(
        "SELECT id, user_id, subject, created_at FROM oauth_identities \
         WHERE provider = $1 AND subject = $2",
    )
    .bind(provider.as_str())
    .bind(subject)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(row.map(|row| OauthIdentity {
        id: row.id,
        user_id: row.user_id,
        provider,
        subject: row.subject,
        created_at: row.created_at,
    }))
}

/// Whether an account already has an identity with this provider.
pub async fn exists_for_user(
    conn: &mut PgConnection,
    user_id: Uuid,
    provider: Provider,
) -> Result<bool, AppError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM oauth_identities WHERE user_id = $1 AND provider = $2)",
    )
    .bind(user_id)
    .bind(provider.as_str())
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::internal)
}

#[allow(clippy::too_many_arguments)]
/// The providers connected to an account, for the account page.
///
/// Names only — the subjects stay server-side. A page needs to render
/// "Google: connected" and offer the other button, nothing more.
pub async fn providers_for_user(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<Vec<Provider>, AppError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT provider FROM oauth_identities WHERE user_id = $1 ORDER BY provider",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    rows.iter()
        .map(|value| Provider::parse(value))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| AppError::internal("unknown provider in oauth_identities"))
}

pub async fn insert(
    conn: &mut PgConnection,
    id: Uuid,
    user_id: Uuid,
    provider: Provider,
    subject: &str,
    firebase_uid: Option<&str>,
    email_at_link: Option<&str>,
    email_verified_at_link: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO oauth_identities \
             (id, user_id, provider, subject, firebase_uid, email_at_link, \
              email_verified_at_link, last_login_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, now())",
    )
    .bind(id)
    .bind(user_id)
    .bind(provider.as_str())
    .bind(subject)
    .bind(firebase_uid)
    .bind(email_at_link)
    .bind(email_verified_at_link)
    .execute(&mut *conn)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db)
            if db.constraint() == Some("oauth_identities_provider_subject_key") =>
        {
            AppError::conflict("That identity is already linked to an account.")
        }
        sqlx::Error::Database(db)
            if db.constraint() == Some("oauth_identities_one_per_provider") =>
        {
            AppError::conflict("This account already has an identity with that provider.")
        }
        _ => AppError::internal(error),
    })?;

    Ok(())
}

pub async fn touch_login(conn: &mut PgConnection, id: Uuid) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE oauth_identities SET last_login_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}
