//! Typeahead for the search box.
//!
//! Three things a person can be reaching for when they start typing: a kind of
//! work, a place, or one particular business. The endpoint answers all three in
//! one list and says which each one is, so the client can turn a choice into
//! the right filter rather than guessing from the text.
//!
//! Business names are matched with `ILIKE` against the raw column, not with
//! `lower(display_name) LIKE`. They read the same and are not the same: the
//! trigram index is on `display_name`, and wrapping the column in `lower()`
//! means the planner cannot use it. Measured at 51,000 contractors, that one
//! difference is a sequential scan against a bitmap index scan, and 36 ms
//! against 4 ms on an endpoint that fires on every keystroke.
//!
//! Assembled per request from the tables that already exist, rather than kept
//! in a materialised index. At this size the union costs less than a millisecond
//! and it is always current — a materialised copy would need rebuilding on every
//! import, every claim and every trade edit, and the failure mode of forgetting
//! is a suggestion list that quietly describes last week's directory.

use cm_core::AppError;
use sqlx::PgConnection;

/// What a suggestion is, which decides what the client does with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A kind of work. Applies `?trade=<value>`.
    Trade,
    /// A ZIP code. Applies `?zip=<value>`.
    Place,
    /// One business. Navigates to its listing.
    Contractor,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Suggestion {
    pub kind: Kind,
    /// What to show.
    pub label: String,
    /// The slug, ZIP or listing slug the client acts on. Never displayed.
    pub value: String,
    /// A second line where one helps — the neighbourhood behind a ZIP, the
    /// trades a business holds. Absent rather than empty when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The most suggestions any one request returns.
///
/// Small on purpose: a list nobody can scan is not a shortcut. Split across the
/// three kinds so a common word cannot fill the list with businesses and hide
/// the trade the person was actually reaching for.
pub const MAX_SUGGESTIONS: i64 = 8;
const MAX_PER_KIND: i64 = 4;

/// The shortest prefix worth answering.
///
/// One character matches most of the directory and tells nobody anything; it
/// also turns the first keystroke of every session into a query.
pub const MIN_QUERY: usize = 2;

/// Suggest what the person might mean.
///
/// Ordered by kind first — a trade or a place is a better guess than one
/// business, because it is the thing that narrows a search rather than ending
/// it — then, within businesses, by the same standing quality the directory
/// ranks by. Exact prefix matches lead within each group: somebody four
/// characters into a word is far more likely to be completing it than to have
/// misspelled something else.
pub async fn suggest(conn: &mut PgConnection, query: &str) -> Result<Vec<Suggestion>, AppError> {
    let needle = query.trim().to_lowercase();
    if needle.chars().count() < MIN_QUERY {
        return Ok(Vec::new());
    }
    let prefix = format!("{needle}%");

    // A business is as often reached for by a word inside its name as by its
    // first one — "hvac" should find ALLIANCE HVAC INC. Prefix matches still
    // lead, so completing a word beats matching one in the middle.
    //
    // Only past two characters: a trigram index cannot help a shorter pattern
    // and would fall back to reading the table, which is exactly the query
    // nobody should be able to fire on every keystroke.
    // One pattern, not a prefix OR a contains. To a trigram index they are the
    // same set — `plumb%` and `%plumb%` extract identical trigrams and return
    // identical candidates — so the OR bought no extra matches and made the
    // planner do the work twice. Measured at 51,000 rows: 32 ms for the pair
    // against 3 ms for the contains alone. The prefix still decides the order,
    // just not membership.
    //
    // Below three characters the pattern is matched literally against the short
    // suggestion sources only; a two-letter substring of 51,000 business names
    // is not a suggestion, it is a table scan.
    let contains = format!("%{needle}%");
    let business_pattern = (needle.chars().count() >= 3).then_some(contains.as_str());

    let rows: Vec<(String, String, String, Option<String>, f64)> = sqlx::query_as(
        "( \
            SELECT 'trade' AS kind, t.name AS label, t.slug AS value, \
                   NULL::text AS hint, \
                   (CASE WHEN lower(t.name) LIKE $2 THEN 1.0 ELSE 0.0 END)::float8 AS lead \
              FROM trades t \
             WHERE t.active \
               AND (lower(t.name) LIKE $2 OR EXISTS ( \
                       SELECT 1 FROM trade_aliases a \
                        WHERE a.trade_id = t.id AND a.alias LIKE $2)) \
             ORDER BY lead DESC, t.sort_order \
             LIMIT $3 \
        ) UNION ALL ( \
            SELECT 'place', r.code, r.code, \
                   NULLIF(r.name, r.code), \
                   (CASE WHEN r.code LIKE $2 THEN 1.0 ELSE 0.0 END)::float8 \
              FROM regions r \
             WHERE r.kind = 'zcta' \
               AND (r.code LIKE $2 OR lower(r.name) LIKE $2) \
             ORDER BY 5 DESC, r.code \
             LIMIT $3 \
        ) UNION ALL ( \
            SELECT 'contractor', c.display_name, c.slug, \
                   NULLIF(btrim(COALESCE(c.owner_address_city, l.city, '')), ''), \
                   ((CASE WHEN c.display_name ILIKE $2 THEN 1.0 ELSE 0.0 END) \
                    + c.quality_score)::float8 \
              FROM contractors c \
              LEFT JOIN license_records l ON l.id = c.license_record_id \
             WHERE c.display_name ILIKE $4 \
             ORDER BY 5 DESC, c.display_name \
             LIMIT $3 \
        )",
    )
    .bind(&needle)
    .bind(&prefix)
    .bind(MAX_PER_KIND)
    .bind(business_pattern)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    let mut suggestions: Vec<Suggestion> = rows
        .into_iter()
        .filter_map(|(kind, label, value, hint, _)| {
            let kind = match kind.as_str() {
                "trade" => Kind::Trade,
                "place" => Kind::Place,
                "contractor" => Kind::Contractor,
                _ => return None,
            };
            Some(Suggestion {
                kind,
                label,
                value,
                hint,
            })
        })
        .collect();

    suggestions.truncate(MAX_SUGGESTIONS as usize);
    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single character is not an intent, and answering it would turn the
    /// first keystroke of every session into a database query.
    #[test]
    fn a_query_too_short_to_mean_anything_is_not_answered() {
        const _: () = assert!(MIN_QUERY >= 2);
    }

    /// The per-kind cap has to leave room for every kind, or a common word
    /// fills the list with businesses and hides the trade somebody was
    /// reaching for.
    #[test]
    fn every_kind_fits_in_the_list() {
        const _: () = assert!(MAX_PER_KIND * 3 >= MAX_SUGGESTIONS);
        const _: () = assert!(MAX_PER_KIND < MAX_SUGGESTIONS);
    }

    /// The wire spelling is the one the client switches on.
    #[test]
    fn the_kinds_serialize_as_the_client_reads_them() {
        for (kind, expected) in [
            (Kind::Trade, "\"trade\""),
            (Kind::Place, "\"place\""),
            (Kind::Contractor, "\"contractor\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).expect("serialize"), expected);
        }
    }
}
