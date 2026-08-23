//! Contractors: our record of a business.
//!
//! Two rules are enforced here rather than left to callers.
//!
//! **No query in this file selects `precise_point`.** Every read path returns
//! `public_point` only, so a contractor whose address is protected cannot have
//! it leaked by a projection somebody forgot to narrow. A test greps this crate
//! to keep it that way.
//!
//! **The importer never writes claimant-owned fields.** `upsert_from_license`
//! touches source-derived columns only, so a refresh cannot overwrite a bio.

use chrono::{DateTime, Utc};
use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

/// Whether a contractor's exact address may be published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressVisibility {
    Protected,
    Public,
}

impl AddressVisibility {
    pub const ALL: [Self; 2] = [Self::Protected, Self::Public];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Protected => "protected",
            Self::Public => "public",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == value)
    }
}

/// Where the published point came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicPointSource {
    Exact,
    ZipCentroid,
    None,
}

impl PublicPointSource {
    pub const ALL: [Self; 3] = [Self::Exact, Self::ZipCentroid, Self::None];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::ZipCentroid => "zip_centroid",
            Self::None => "none",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == value)
    }
}

/// Trade provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeSource {
    Cslb,
    SelfReported,
}

impl TradeSource {
    pub const ALL: [Self; 2] = [Self::Cslb, Self::SelfReported];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cslb => "cslb",
            Self::SelfReported => "self_reported",
        }
    }
}

/// What the importer knows about a contractor.
#[derive(Debug, Clone)]
pub struct SourceFacts {
    pub license_record_id: Uuid,
    pub display_name: String,
    pub slug: String,
    pub postal_code: Option<String>,
    pub region_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpsertResult {
    pub id: Uuid,
    pub created: bool,
    /// True when the postal code moved, so the caller knows to re-locate.
    pub location_changed: bool,
}

/// Create or refresh a contractor from licence data.
pub async fn upsert_from_license(
    conn: &mut PgConnection,
    facts: &SourceFacts,
) -> Result<UpsertResult, AppError> {
    let existing: Option<(Uuid, Option<String>, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, postal_code, claimed_by_user_id FROM contractors WHERE license_record_id = $1",
    )
    .bind(facts.license_record_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    match existing {
        None => {
            let id = new_id();
            sqlx::query(
                "INSERT INTO contractors \
                     (id, license_record_id, display_name, slug, postal_code, region_id) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id)
            .bind(facts.license_record_id)
            .bind(&facts.display_name)
            .bind(&facts.slug)
            .bind(&facts.postal_code)
            .bind(facts.region_id)
            .execute(&mut *conn)
            .await
            .map_err(AppError::internal)?;

            Ok(UpsertResult {
                id,
                created: true,
                location_changed: facts.postal_code.is_some(),
            })
        }
        Some((id, postal_code, claimed_by)) => {
            // The display name is source-derived only until someone claims the
            // listing; after that it is theirs, and a refresh must not take it
            // back. Postal code stays source-derived either way: it is the
            // licence address, not a profile field.
            if claimed_by.is_none() {
                sqlx::query(
                    "UPDATE contractors SET display_name = $2, updated_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(&facts.display_name)
                .execute(&mut *conn)
                .await
                .map_err(AppError::internal)?;
            }

            let location_changed = postal_code != facts.postal_code;
            if location_changed {
                sqlx::query(
                    "UPDATE contractors SET postal_code = $2, region_id = $3, updated_at = now() \
                      WHERE id = $1",
                )
                .bind(id)
                .bind(&facts.postal_code)
                .bind(facts.region_id)
                .execute(&mut *conn)
                .await
                .map_err(AppError::internal)?;
            }

            Ok(UpsertResult {
                id,
                created: false,
                location_changed,
            })
        }
    }
}

/// Replace the imported trade set for a contractor.
///
/// Scoped to `source = 'cslb'`, so a claimant's self-reported trades survive an
/// import untouched.
pub async fn replace_cslb_trades(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    trade_ids: &[Uuid],
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM contractor_trades WHERE contractor_id = $1 AND source = 'cslb'")
        .bind(contractor_id)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    for trade_id in trade_ids {
        sqlx::query(
            "INSERT INTO contractor_trades (contractor_id, trade_id, source) \
             VALUES ($1, $2, 'cslb') ON CONFLICT DO NOTHING",
        )
        .bind(contractor_id)
        .bind(trade_id)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;
    }

    Ok(())
}

/// Set the published point, and the precise one when the address is public.
///
/// The published point is what every read path uses, including distance search.
/// If search ran on the precise point while the map showed a centroid, the
/// radius filter could be binary-searched to recover the protected address.
pub async fn set_location(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    precise: Option<(f64, f64)>,
    public: Option<(f64, f64)>,
    public_source: PublicPointSource,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE contractors SET \
             precise_point = CASE WHEN $2::float8 IS NULL THEN NULL \
                 ELSE ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography END, \
             public_point = CASE WHEN $4::float8 IS NULL THEN NULL \
                 ELSE ST_SetSRID(ST_MakePoint($4, $5), 4326)::geography END, \
             public_point_source = $6, \
             updated_at = now() \
          WHERE id = $1",
    )
    .bind(contractor_id)
    .bind(precise.map(|(lon, _)| lon))
    .bind(precise.map(|(_, lat)| lat))
    .bind(public.map(|(lon, _)| lon))
    .bind(public.map(|(_, lat)| lat))
    .bind(public_source.as_str())
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Everything the location service needs to decide where a contractor appears.
#[derive(Debug, Clone)]
pub struct LocationInputs {
    pub id: Uuid,
    pub address_visibility: AddressVisibility,
    pub postal_code: Option<String>,
    pub is_claimed: bool,
}

pub async fn location_inputs(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Option<LocationInputs>, AppError> {
    let row: Option<(Uuid, String, Option<String>, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, address_visibility, postal_code, claimed_by_user_id \
           FROM contractors WHERE id = $1",
    )
    .bind(contractor_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    row.map(|(id, visibility, postal_code, claimed_by)| {
        Ok(LocationInputs {
            id,
            address_visibility: AddressVisibility::parse(&visibility).ok_or_else(|| {
                AppError::internal(format!("unknown address visibility: {visibility}"))
            })?,
            postal_code,
            is_claimed: claimed_by.is_some(),
        })
    })
    .transpose()
}

/// The address parts held on a licence record.
type AddressParts = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The address the geocoder should resolve, assembled from the licence record.
pub async fn geocodable_address(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Option<String>, AppError> {
    let row: Option<AddressParts> = sqlx::query_as(
        "SELECT l.address_line1, l.city, l.state, l.postal_code \
           FROM contractors c JOIN license_records l ON l.id = c.license_record_id \
          WHERE c.id = $1",
    )
    .bind(contractor_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(row.and_then(|(line1, city, state, postal)| {
        let parts: Vec<String> = [line1, city, state, postal]
            .into_iter()
            .flatten()
            .map(|part| part.trim().to_owned())
            .filter(|part| !part.is_empty())
            .collect();

        (!parts.is_empty()).then(|| parts.join(", "))
    }))
}

/// Attach a claim. Returns false if the listing was claimed in the meantime.
pub async fn attach_claimant(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE contractors SET claimed_by_user_id = $2, claimed_at = now(), updated_at = now() \
          WHERE id = $1 AND claimed_by_user_id IS NULL",
    )
    .bind(contractor_id)
    .bind(user_id)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

/// The contractor a user has claimed, if any.
pub async fn claimed_by(conn: &mut PgConnection, user_id: Uuid) -> Result<Option<Uuid>, AppError> {
    sqlx::query_scalar("SELECT id FROM contractors WHERE claimed_by_user_id = $1")
        .bind(user_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// Fields a claimant may edit. `verified` is deliberately absent: it is
/// computed, and the handler rejects a request that mentions it.
#[derive(Debug, Clone, Default)]
pub struct ProfileUpdate {
    pub bio: Option<String>,
    pub website_url: Option<String>,
    pub public_phone: Option<String>,
    pub accepts_dm: Option<bool>,
    pub address_visibility: Option<AddressVisibility>,
}

pub async fn update_profile(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    update: &ProfileUpdate,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE contractors SET \
             bio = COALESCE($2, bio), \
             website_url = COALESCE($3, website_url), \
             public_phone = COALESCE($4, public_phone), \
             accepts_dm = COALESCE($5, accepts_dm), \
             address_visibility = COALESCE($6, address_visibility), \
             updated_at = now() \
          WHERE id = $1",
    )
    .bind(contractor_id)
    .bind(&update.bio)
    .bind(&update.website_url)
    .bind(&update.public_phone)
    .bind(update.accepts_dm)
    .bind(update.address_visibility.map(|v| v.as_str()))
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Write the verified badge. Called from exactly one place in the codebase.
pub async fn set_verification(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    verified: bool,
    reason: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE contractors SET \
             verified = $2, \
             verified_at = CASE WHEN $2 THEN now() ELSE NULL END, \
             verification_reason = $3, \
             updated_at = now() \
          WHERE id = $1",
    )
    .bind(contractor_id)
    .bind(verified)
    .bind(reason)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Contractors linked to a licence record, for post-import recomputation.
pub async fn ids_for_license(
    conn: &mut PgConnection,
    license_record_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query_scalar("SELECT id FROM contractors WHERE license_record_id = $1")
        .bind(license_record_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// Whether a contractor accepts direct messages, and who owns it.
#[derive(Debug, Clone, Copy)]
pub struct MessagingTarget {
    pub claimed_by_user_id: Option<Uuid>,
    pub accepts_dm: bool,
}

pub async fn messaging_target(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Option<MessagingTarget>, AppError> {
    let row: Option<(Option<Uuid>, bool)> =
        sqlx::query_as("SELECT claimed_by_user_id, accepts_dm FROM contractors WHERE id = $1")
            .bind(contractor_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(AppError::internal)?;

    Ok(row.map(|(claimed_by_user_id, accepts_dm)| MessagingTarget {
        claimed_by_user_id,
        accepts_dm,
    }))
}

/// A contractor as the public sees it. No precise coordinates, by construction.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicContractor {
    pub id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub verified: bool,
    pub verified_at: Option<DateTime<Utc>>,
    pub bio: Option<String>,
    pub website_url: Option<String>,
    pub public_phone: Option<String>,
    pub postal_code: Option<String>,
    pub accepts_dm: bool,
    pub is_claimed: bool,
    /// Published location. `None` when the contractor has not been located yet.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// How precise the published point is, so a client can say so honestly.
    pub location_precision: PublicPointSource,
    pub license_no: Option<String>,
    pub license_status: Option<String>,
    pub trades: Vec<String>,
    pub distance_m: Option<f64>,
}
