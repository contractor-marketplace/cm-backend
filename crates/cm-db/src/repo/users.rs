//! Accounts and roles.

use chrono::{DateTime, Utc};
use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

/// Account lifecycle. Mirrors the `users.status` CHECK constraint; a migration
/// test asserts the two agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UserStatus {
    Active,
    Suspended,
    Deleted,
}

impl UserStatus {
    pub const ALL: [Self; 3] = [Self::Active, Self::Suspended, Self::Deleted];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
            .ok_or_else(|| AppError::internal(format!("unknown user status in database: {value}")))
    }

    /// Only an active account may authenticate.
    pub fn can_authenticate(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// Which side of the marketplace an account is on.
///
/// Chosen once at registration and never changed. Deliberately not a role:
/// roles are additive and granted — `Role::Contractor` is granted when a
/// moderator approves a claim, and means the holder proved they own a licensed
/// business. This is a single mutually exclusive value, which is what makes
/// "an account is never both" something the type system can state.
///
/// Mirrors the `users.account_type` CHECK constraint; a migration test asserts
/// the two agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    Homeowner,
    Contractor,
}

impl AccountType {
    pub const ALL: [Self; 2] = [Self::Homeowner, Self::Contractor];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Homeowner => "homeowner",
            Self::Contractor => "contractor",
        }
    }

    /// For a value arriving from a client, so the error is a 400 rather than a
    /// 500 and names what was expected.
    pub fn parse_request(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| {
                AppError::invalid(format!(
                    "Account type must be one of: {}.",
                    Self::ALL
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// For a value read back out of the database, where an unknown value is a
    /// bug in this code rather than bad input.
    pub fn parse(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| AppError::internal(format!("unknown account type in database: {value}")))
    }

    /// Claiming a listing is the contractor's side of the marketplace.
    pub fn may_claim_a_listing(self) -> bool {
        matches!(self, Self::Contractor)
    }

    /// Starting a conversation, and holding a homeowner profile, are the
    /// homeowner's side. A contractor replies within a conversation a
    /// homeowner opened; it never opens one.
    pub fn may_hire(self) -> bool {
        matches!(self, Self::Homeowner)
    }
}

/// Authorization roles. Mirrors the `user_roles.role` CHECK constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Homeowner,
    Contractor,
    Moderator,
    Admin,
}

impl Role {
    pub const ALL: [Self; 4] = [
        Self::Homeowner,
        Self::Contractor,
        Self::Moderator,
        Self::Admin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Homeowner => "homeowner",
            Self::Contractor => "contractor",
            Self::Moderator => "moderator",
            Self::Admin => "admin",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == value)
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: Uuid,
    pub email: Option<String>,
    pub display_name: String,
    pub status: UserStatus,
    /// Which side of the marketplace. Fixed at registration.
    pub account_type: AccountType,
    pub email_verified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: Uuid,
    email: Option<String>,
    display_name: String,
    status: String,
    account_type: String,
    email_verified_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl TryFrom<UserRow> for User {
    type Error = AppError;

    fn try_from(row: UserRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            email: row.email,
            display_name: row.display_name,
            status: UserStatus::parse(&row.status)?,
            account_type: AccountType::parse(&row.account_type)?,
            email_verified_at: row.email_verified_at,
            created_at: row.created_at,
        })
    }
}

const SELECT_USER: &str = "SELECT id, email, display_name, status, account_type, \
     email_verified_at, created_at FROM users";

/// Insert a new account.
///
/// `email` must already be trimmed: the CHECK constraint rejects surrounding
/// whitespace rather than quietly storing it, and `email_norm` — the generated
/// column the unique index is built on — handles case only. `None` is a
/// federated account whose provider shared no address (0035): it collides with
/// nothing — the unique index ignores NULLs — and adds an address later.
pub async fn insert(
    conn: &mut PgConnection,
    id: Uuid,
    email: Option<&str>,
    display_name: &str,
    account_type: AccountType,
) -> Result<User, AppError> {
    let row: UserRow = sqlx::query_as(
        "INSERT INTO users (id, email, display_name, account_type) VALUES ($1, $2, $3, $4) \
         RETURNING id, email, display_name, status, account_type, email_verified_at, created_at",
    )
    .bind(id)
    .bind(email)
    .bind(display_name)
    .bind(account_type.as_str())
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db) if db.constraint() == Some("users_email_norm_key") => {
            AppError::conflict("That email address is already registered.")
        }
        _ => AppError::internal(error),
    })?;

    row.try_into()
}

/// Set a proved address on an account.
///
/// Called only after the emailed code came back, so the address and its
/// verification are one write — there is no state where the new address is
/// stored but unproved. The unique index arbitrates collisions here exactly as
/// it does at registration, and with the same message.
pub async fn update_email(
    conn: &mut PgConnection,
    user_id: Uuid,
    email: &str,
) -> Result<User, AppError> {
    let row: UserRow = sqlx::query_as(
        "UPDATE users SET email = $2, email_verified_at = now(), updated_at = now() \
          WHERE id = $1 \
      RETURNING id, email, display_name, status, account_type, email_verified_at, created_at",
    )
    .bind(user_id)
    .bind(email)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db) if db.constraint() == Some("users_email_norm_key") => {
            AppError::conflict("That email address is already registered.")
        }
        _ => AppError::internal(error),
    })?;

    row.try_into()
}

/// Look an account up by email.
///
/// Normalisation is applied in SQL by the same expression the generated column
/// uses, so the lookup and the uniqueness constraint can never disagree about
/// what counts as the same address.
pub async fn find_by_email(conn: &mut PgConnection, email: &str) -> Result<Option<User>, AppError> {
    let row: Option<UserRow> = sqlx::query_as(&format!(
        "{SELECT_USER} WHERE email_norm = lower(btrim($1))"
    ))
    .bind(email)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    row.map(User::try_from).transpose()
}

pub async fn find_by_id(conn: &mut PgConnection, id: Uuid) -> Result<Option<User>, AppError> {
    let row: Option<UserRow> = sqlx::query_as(&format!("{SELECT_USER} WHERE id = $1"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    row.map(User::try_from).transpose()
}

/// Record that this account's address is verified. Idempotent, and the only
/// writer of `email_verified_at`: the first proof of inbox control wins, and
/// nothing ever un-verifies an address short of changing it.
pub async fn mark_email_verified(conn: &mut PgConnection, user_id: Uuid) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE users SET email_verified_at = now(), updated_at = now() \
          WHERE id = $1 AND email_verified_at IS NULL",
    )
    .bind(user_id)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

pub async fn roles(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<Role>, AppError> {
    let names: Vec<String> =
        sqlx::query_scalar("SELECT role FROM user_roles WHERE user_id = $1 ORDER BY role")
            .bind(user_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(AppError::internal)?;

    names
        .iter()
        .map(|name| {
            Role::parse(name)
                .ok_or_else(|| AppError::internal(format!("unknown role in database: {name}")))
        })
        .collect()
}

/// Grant a role. Returns false when the account already held it, so callers can
/// avoid writing a misleading audit entry for a no-op.
pub async fn grant_role(
    conn: &mut PgConnection,
    user_id: Uuid,
    role: Role,
    granted_by: Option<Uuid>,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "INSERT INTO user_roles (user_id, role, granted_by) VALUES ($1, $2, $3) \
         ON CONFLICT (user_id, role) DO NOTHING",
    )
    .bind(user_id)
    .bind(role.as_str())
    .bind(granted_by)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

/// Revoke a role. Returns false when the account did not hold it.
pub async fn revoke_role(
    conn: &mut PgConnection,
    user_id: Uuid,
    role: Role,
) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM user_roles WHERE user_id = $1 AND role = $2")
        .bind(user_id)
        .bind(role.as_str())
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}
