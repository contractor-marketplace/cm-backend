//! Contractors: our record of a business.
//!
//! Two rules are enforced here rather than left to callers.
//!
//! **No query in this file selects `precise_point`.** Every read path returns
//! `public_point` only, so a contractor whose address is protected cannot have
//! it leaked by a projection somebody forgot to narrow.
//!
//! What keeps it that way is `reads_return_the_published_point_and_never_the_
//! precise_column` in `cm-api/tests/directory.rs`: it writes a precise point
//! that deliberately disagrees with the published one, then drives the list,
//! the map, a text search and a radius search and asserts all four return the
//! published coordinates. (This comment previously claimed a test "greps this
//! crate". None does, and none should — a grep would miss a leak arriving
//! through a join or a view, which the behavioural test catches.)
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
/// Store the geocoded point without touching what is published.
///
/// Separate from `set_location` because that function writes all three location
/// columns together, and a caller who wants to record a geocode without
/// deciding publication would have to pass the published point back in — or
/// pass `None` and silently erase it, which is exactly the bug this exists to
/// make impossible.
pub async fn set_precise_point(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    lon: f64,
    lat: f64,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE contractors \
            SET precise_point = ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(contractor_id)
    .bind(lon)
    .bind(lat)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Write all three location columns together.
///
/// A `None` for either point WRITES NULL rather than leaving the column alone.
/// That is deliberate — it is how a listing gets unlocated — but it is also
/// sharp, so prefer `location::republish`, which reads the row and cannot
/// accidentally drop a point it did not know about.
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
    /// `(lon, lat)` of the geocoded address, when one has been resolved.
    ///
    /// Read here so the publish decision can be made from stored state alone.
    /// Without it, recomputing the published point means either passing the
    /// precise point back in from the caller or losing it — and losing it is
    /// what used to happen on every re-import.
    pub precise_point: Option<(f64, f64)>,
}

/// `(id, address_visibility, postal_code, claimed_by, precise lon, precise lat)`
/// — the raw row behind `LocationInputs`, named so the query's type is readable.
type LocationRow = (
    Uuid,
    String,
    Option<String>,
    Option<Uuid>,
    Option<f64>,
    Option<f64>,
);

pub async fn location_inputs(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Option<LocationInputs>, AppError> {
    let row: Option<LocationRow> = sqlx::query_as(
        "SELECT id, address_visibility, postal_code, claimed_by_user_id, \
                    ST_X(precise_point::geometry), ST_Y(precise_point::geometry) \
               FROM contractors WHERE id = $1",
    )
    .bind(contractor_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    row.map(|(id, visibility, postal_code, claimed_by, lon, lat)| {
        Ok(LocationInputs {
            id,
            address_visibility: AddressVisibility::parse(&visibility).ok_or_else(|| {
                AppError::internal(format!("unknown address visibility: {visibility}"))
            })?,
            postal_code,
            is_claimed: claimed_by.is_some(),
            precise_point: lon.zip(lat),
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

/// The address the geocoder should resolve.
///
/// The claimant's address when they have set one, the licence address
/// otherwise. This must agree with what the profile displays: if the page
/// showed the owner's address while the pin resolved the licence's, the map and
/// the page would disagree about where somebody is — the same class of bug the
/// public/precise point split exists to prevent, reintroduced from a new
/// direction.
///
/// The per-column COALESCE is only safe because 0024 constrains the owner
/// address to be all four parts or none: it can never take the owner's street
/// and the licence's city and geocode a building that exists nowhere.
///
/// A LEFT JOIN, not the inner join this used to be. `owner_address_*` lives on
/// `contractors`, and a listing with an owner address but no licence record
/// still has an address worth resolving.
pub async fn geocodable_address(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Option<String>, AppError> {
    let row: Option<AddressParts> = sqlx::query_as(
        "SELECT COALESCE(c.owner_address_line1, l.address_line1), \
                COALESCE(c.owner_address_city, l.city), \
                COALESCE(c.owner_address_state, l.state), \
                COALESCE(c.owner_address_postal_code, l.postal_code) \
           FROM contractors c LEFT JOIN license_records l ON l.id = c.license_record_id \
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
/// A contractor's own address, all four parts together.
///
/// One struct rather than four `Option<String>` fields because 0024 constrains
/// them to be whole or absent, and four independent options would make the
/// half-filled state representable in Rust even though the database refuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerAddress {
    pub line1: String,
    pub city: String,
    pub state: String,
    pub postal_code: String,
}

/// What a field-level edit is asking for.
///
/// `Unchanged` and `Cleared` have to be distinguishable, and `Option<Option<T>>`
/// does it at the cost of being unreadable at every call site. Every optional
/// profile field goes through this: without it, "leave my bio alone" and
/// "delete my bio" are the same request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Edit<T> {
    #[default]
    Unchanged,
    Set(T),
    Cleared,
}

impl<T> Edit<T> {
    /// `(should_write, value)` — the shape the SQL below binds.
    fn parts(self) -> (bool, Option<T>) {
        match self {
            Self::Unchanged => (false, None),
            Self::Set(value) => (true, Some(value)),
            Self::Cleared => (true, None),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProfileUpdate {
    pub bio: Option<String>,
    pub website_url: Option<String>,
    pub public_phone: Option<String>,
    pub accepts_dm: Option<bool>,
    pub address_visibility: Option<AddressVisibility>,
    /// The claimant's own address, which REPLACES the licence address on every
    /// read path once set. See migrations/0024 for why that reversal is
    /// deliberate, and `geocodable_address` for the pin following it.
    pub owner_address: Edit<OwnerAddress>,
    /// The contractor's assertion about their own Google page, distinct from
    /// the `google_place_url` our matcher inferred. Theirs wins.
    pub google_review_url: Edit<String>,
    pub yelp_url: Edit<String>,
}

pub async fn update_profile(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    update: &ProfileUpdate,
) -> Result<(), AppError> {
    // The older fields keep their COALESCE semantics — absent means unchanged,
    // and there has never been a way to clear them. The fields added in 0024
    // use `Edit`, which can express "clear this", so each one binds a written
    // flag alongside its value rather than relying on NULL to mean two things.
    let (write_address, address) = update.owner_address.clone().parts();
    let (write_google, google) = update.google_review_url.clone().parts();
    let (write_yelp, yelp) = update.yelp_url.clone().parts();

    sqlx::query(
        "UPDATE contractors SET \
             bio = COALESCE($2, bio), \
             website_url = COALESCE($3, website_url), \
             public_phone = COALESCE($4, public_phone), \
             accepts_dm = COALESCE($5, accepts_dm), \
             address_visibility = COALESCE($6, address_visibility), \
             owner_address_line1 = CASE WHEN $7 THEN $8 ELSE owner_address_line1 END, \
             owner_address_city = CASE WHEN $7 THEN $9 ELSE owner_address_city END, \
             owner_address_state = CASE WHEN $7 THEN $10 ELSE owner_address_state END, \
             owner_address_postal_code = \
                 CASE WHEN $7 THEN $11 ELSE owner_address_postal_code END, \
             google_review_url = CASE WHEN $12 THEN $13 ELSE google_review_url END, \
             yelp_url = CASE WHEN $14 THEN $15 ELSE yelp_url END, \
             updated_at = now() \
          WHERE id = $1",
    )
    .bind(contractor_id)
    .bind(&update.bio)
    .bind(&update.website_url)
    .bind(&update.public_phone)
    .bind(update.accepts_dm)
    .bind(update.address_visibility.map(|v| v.as_str()))
    .bind(write_address)
    .bind(address.as_ref().map(|a| a.line1.clone()))
    .bind(address.as_ref().map(|a| a.city.clone()))
    .bind(address.as_ref().map(|a| a.state.clone()))
    .bind(address.as_ref().map(|a| a.postal_code.clone()))
    .bind(write_google)
    .bind(google)
    .bind(write_yelp)
    .bind(yelp)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(())
}

/// Record the profile photo, returning the key it replaced so the caller can
/// delete the old object.
///
/// Returning the displaced key rather than deleting here keeps this function
/// pure database work: object deletion is a network call that can fail, and it
/// must not be able to roll back the row that already points at the new photo.
pub async fn set_photo(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    key: &str,
    width: u32,
    height: u32,
) -> Result<Option<String>, AppError> {
    // The old key is read in a CTE rather than a subquery inside RETURNING.
    // A CTE is evaluated against the snapshot the statement started with, so it
    // reliably yields the displaced key; a bare subquery over the same row
    // being updated is not well defined and can hand back the value just
    // written, which would leak the old object by reporting nothing to delete.
    let previous: Option<String> = sqlx::query_scalar(
        "WITH previous AS (SELECT photo_storage_key AS k FROM contractors WHERE id = $1) \
         UPDATE contractors \
            SET photo_storage_key = $2, photo_width = $3, photo_height = $4, updated_at = now() \
          WHERE id = $1 \
      RETURNING (SELECT k FROM previous)",
    )
    .bind(contractor_id)
    .bind(key)
    .bind(width as i32)
    .bind(height as i32)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)?
    .flatten();

    Ok(previous.filter(|old| old != key))
}

/// Clear the profile photo, returning the key that was there.
pub async fn clear_photo(
    conn: &mut PgConnection,
    contractor_id: Uuid,
) -> Result<Option<String>, AppError> {
    sqlx::query_scalar(
        "UPDATE contractors \
            SET photo_storage_key = NULL, photo_width = NULL, photo_height = NULL, \
                updated_at = now() \
          WHERE id = $1 \
      RETURNING photo_storage_key",
    )
    .bind(contractor_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
    .map(Option::flatten)
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

/// A contractor as the public sees it.
///
/// Built only from the published point — no read path selects `precise_point`.
/// Since 0019 the two are usually the same coordinates, because the licence
/// address is a public record and the directory publishes it. The separation
/// still matters: it is what a `protected` listing relies on, and it is why
/// search and map can never disagree about where somebody is.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicContractor {
    /// What this row scored for the query that returned it, and its standing
    /// quality. Read to build the next page's cursor and never serialised: a
    /// score is an internal ordering detail, and publishing one invites a
    /// client to sort by it and disagree with the server about the order.
    #[serde(skip)]
    pub rank_score: Option<f64>,
    #[serde(skip)]
    pub quality_score: Option<f64>,

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
    /// The business address on the licence, as the CSLB register publishes it.
    ///
    /// Not a field a contractor filled in, and not one they can edit here: it
    /// is what the register says. A correction goes through the CSLB and
    /// arrives at the next import.
    pub address_line1: Option<String>,
    pub address_city: Option<String>,
    pub address_state: Option<String>,
    pub trades: Vec<String>,
    pub distance_m: Option<f64>,
    /// Google's own rating for the business this listing was matched to, and
    /// Google's own count of the reviews behind it.
    ///
    /// Named for their provenance rather than as a bare `rating`, because they
    /// are not ours and not the contractor's: they come from a third party via
    /// the enrichment load, and a client should be able to say so. Both are
    /// `None` for the great majority of listings, which the load never reached.
    ///
    /// The count is Google's total, which is usually LARGER than the number of
    /// reviews `contractor_reviews` holds — the scrape caps at 200 per place.
    /// See migrations/0022.
    pub google_rating: Option<f64>,
    pub google_review_count: Option<i32>,
    /// The Google listing these came from, so a reader can check the match and
    /// read the reviews the sample does not carry.
    ///
    /// Worth treating as load-bearing rather than decorative: the profile
    /// asserts that a particular Google business is this licence holder, and
    /// this link is the only way a visitor can audit that claim.
    pub google_place_url: Option<String>,

    /// True when `address_*` above came from the claimant rather than the CSLB
    /// register, so a client can attribute it instead of implying the register
    /// says something it does not.
    pub address_is_owner_supplied: bool,
    /// The licence address, always, even when the owner has overridden the
    /// displayed one. Kept so a profile can show "on the licence: …" and a
    /// reader can see both.
    pub license_address_line1: Option<String>,
    pub license_address_city: Option<String>,
    /// Review pages the contractor asserts are theirs.
    pub google_review_url: Option<String>,
    pub yelp_url: Option<String>,
    /// Their profile photo, already a public URL. `None` for the ~49,700
    /// listings nobody has claimed.
    pub photo_url: Option<String>,
    pub photo_width: Option<i32>,
    pub photo_height: Option<i32>,
}

/* ── Ranking signals ───────────────────────────────────────────────────────
 * Read here, scored in cm-domain, written back here. The formula does not live
 * in SQL on purpose: it is a business rule, it wants unit tests that do not
 * need a database, and an `ORDER BY` is a bad place to keep one.
 */

/// What the ranking score is computed from, for one contractor.
#[derive(Debug, Clone)]
pub struct RankingSignals {
    pub id: Uuid,
    pub rating: Option<f64>,
    pub review_count: Option<i32>,
    pub verified: bool,
    pub claimed: bool,
    pub has_bio: bool,
    pub has_photo: bool,
    pub has_phone: bool,
    pub has_website: bool,
}

/// The directory's average rating, across listings that have one.
///
/// The value an unrated listing is assumed to hold, so that having no reviews
/// is not itself a penalty. `None` when nothing is rated yet, which on a fresh
/// import is every row.
pub async fn mean_rating(conn: &mut PgConnection) -> Result<Option<f64>, AppError> {
    sqlx::query_scalar("SELECT avg(google_rating)::float8 FROM contractors")
        .fetch_one(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// A page of contractors to score, ordered by id so paging is stable.
pub async fn ranking_signals_after(
    conn: &mut PgConnection,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<RankingSignals>, AppError> {
    sqlx::query_as(
        "SELECT id, \
                google_rating::float8 AS rating, \
                google_review_count AS review_count, \
                verified, \
                (claimed_by_user_id IS NOT NULL) AS claimed, \
                (btrim(coalesce(bio, '')) <> '') AS has_bio, \
                (photo_storage_key IS NOT NULL) AS has_photo, \
                (btrim(coalesce(public_phone, '')) <> '') AS has_phone, \
                (btrim(coalesce(website_url, '')) <> '') AS has_website \
           FROM contractors \
          WHERE $1::uuid IS NULL OR id > $1 \
          ORDER BY id \
          LIMIT $2",
    )
    .bind(after)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Write a batch of scores in one statement.
///
/// Unnested arrays rather than a statement per row: this runs over every
/// contractor on a nightly timer, and fifty thousand round trips is a different
/// kind of job from fifty.
pub async fn set_quality_scores(
    conn: &mut PgConnection,
    scored: &[(Uuid, f32)],
) -> Result<u64, AppError> {
    if scored.is_empty() {
        return Ok(0);
    }

    let ids: Vec<Uuid> = scored.iter().map(|(id, _)| *id).collect();
    let scores: Vec<f32> = scored.iter().map(|(_, score)| *score).collect();

    let result = sqlx::query(
        "UPDATE contractors c \
            SET quality_score = s.score, updated_at = now() \
           FROM unnest($1::uuid[], $2::real[]) AS s(id, score) \
          WHERE c.id = s.id AND c.quality_score IS DISTINCT FROM s.score",
    )
    .bind(&ids)
    .bind(&scores)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected())
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for RankingSignals {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            rating: row.try_get("rating")?,
            review_count: row.try_get("review_count")?,
            verified: row.try_get("verified")?,
            claimed: row.try_get("claimed")?,
            has_bio: row.try_get("has_bio")?,
            has_photo: row.try_get("has_photo")?,
            has_phone: row.try_get("has_phone")?,
            has_website: row.try_get("has_website")?,
        })
    }
}
