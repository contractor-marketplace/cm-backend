//! Third-party reviews published against a contractor.
//!
//! These are not reviews written on this site. Nobody can post one here; there
//! is no write path in this module on purpose. They are Google Maps reviews
//! collected by `tools/gmaps-enrichment` and promoted into `contractor_reviews`
//! by `publish.sql`, and every read path names the source so the product can
//! attribute them rather than passing them off as its own.
//!
//! The summary — rating and total — is not here: it lives denormalised on
//! `contractors` because the directory list and map need it in the same
//! projection they already build. See `contractors::PublicContractor`.

use cm_core::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

/// Where a review came from.
///
/// One variant today. It exists as an enum anyway because the column is
/// TEXT + CHECK per the house convention, and a test pins this list against the
/// constraint in the catalogue — the pairing is what stops the two
/// hand-written vocabularies from drifting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSource {
    Google,
}

impl ReviewSource {
    pub const ALL: [Self; 1] = [Self::Google];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == value)
    }
}

/// A review as the public sees it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicReview {
    pub source: ReviewSource,
    pub author_name: Option<String>,
    pub rating: f64,
    pub body: Option<String>,
    /// The age as the source phrased it — "a year ago" — not a date.
    ///
    /// The scraper never received an absolute timestamp, so there is no date to
    /// give. Parsing the phrase into one would manufacture a precision the
    /// source does not have. See migrations/0022.
    pub relative_age: Option<String>,
    /// The business owner's public reply, when they left one.
    pub owner_reply: Option<String>,
    /// How many photos the reviewer attached. The photos themselves are not
    /// republished: they are hosted by the source, and hotlinking someone
    /// else's images is both a privacy question and a dependency on a URL that
    /// can rot.
    pub photo_count: i32,
}

/// The cap on how many reviews a profile will return.
///
/// The scrape holds up to 200 for a place. Serving all of them would make the
/// profile response several hundred kilobytes to render a page nobody scrolls
/// to the end of.
pub const MAX_PER_CONTRACTOR: i64 = 30;

/// `(source, author_name, rating, body, relative_age, owner_reply, photo_count)`
/// — the raw row behind `PublicReview`, named so the query's type is readable.
type ReviewRow = (
    String,
    Option<String>,
    f64,
    Option<String>,
    Option<String>,
    Option<String>,
    i32,
);

/// Reviews for one contractor, in the order the source ranked them.
///
/// That ordering is `position`, which is Google's own "most relevant" sequence.
/// It is not chronological and is not presented as such — there are no dates to
/// sort by.
pub async fn list_for_contractor(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    limit: i64,
) -> Result<Vec<PublicReview>, AppError> {
    let limit = limit.clamp(1, MAX_PER_CONTRACTOR);

    let rows: Vec<ReviewRow> = sqlx::query_as(
        "SELECT source, author_name, rating::float8, body, relative_age, owner_reply, \
                    photo_count \
               FROM contractor_reviews \
              WHERE contractor_id = $1 \
              ORDER BY position \
              LIMIT $2",
    )
    .bind(contractor_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    rows.into_iter()
        .map(
            |(source, author_name, rating, body, relative_age, owner_reply, photo_count)| {
                Ok(PublicReview {
                    source: ReviewSource::parse(&source).ok_or_else(|| {
                        AppError::internal(format!("unknown review source: {source}"))
                    })?,
                    author_name,
                    rating,
                    body,
                    relative_age,
                    owner_reply,
                    photo_count,
                })
            },
        )
        .collect()
}
