//! What people did with the results.
//!
//! A golden set says whether search finds the right things; it cannot say
//! whether anyone clicks them. This is the other half of that question, and it
//! is written before the ranking that needs it — interaction data cannot be
//! backfilled, so the clock starts when the logging ships, not when the
//! ranking does.
//!
//! Writes are best-effort by construction: `record` takes a connection and
//! returns nothing a caller has to handle, and every call site drops the
//! result. A failure to log an impression must never fail the search that
//! produced it.

use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Shown on a page somebody saw.
    Impression,
    /// Opened.
    Click,
    /// Acted on — a message started.
    Contact,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Impression, Self::Click, Self::Contact];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Impression => "impression",
            Self::Click => "click",
            Self::Contact => "contact",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Directory,
    Jobs,
}

impl Surface {
    pub const ALL: [Self; 2] = [Self::Directory, Self::Jobs];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Jobs => "jobs",
        }
    }
}

/// One thing that happened, in the terms the ranking is judged by.
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: Kind,
    pub surface: Surface,
    pub subject_id: Uuid,
    pub actor_user_id: Option<Uuid>,
    /// One-based rank on the page it appeared on.
    pub position: i32,
    pub had_query: bool,
    pub sort: Option<String>,
    /// Ties an impression to the click that followed it.
    pub request_id: Option<String>,
}

/// Write a page's worth of events in one statement.
///
/// One row per result shown is a lot of rows and no round trips: a page of
/// twenty is twenty values in one insert, not twenty inserts.
pub async fn record(conn: &mut PgConnection, events: &[Event]) -> Result<u64, AppError> {
    if events.is_empty() {
        return Ok(0);
    }

    let ids: Vec<Uuid> = events.iter().map(|_| new_id()).collect();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let surfaces: Vec<&str> = events.iter().map(|e| e.surface.as_str()).collect();
    let subjects: Vec<Uuid> = events.iter().map(|e| e.subject_id).collect();
    let actors: Vec<Option<Uuid>> = events.iter().map(|e| e.actor_user_id).collect();
    let positions: Vec<i32> = events.iter().map(|e| e.position).collect();
    let queried: Vec<bool> = events.iter().map(|e| e.had_query).collect();
    let sorts: Vec<Option<String>> = events.iter().map(|e| e.sort.clone()).collect();
    let requests: Vec<Option<String>> = events.iter().map(|e| e.request_id.clone()).collect();

    let result = sqlx::query(
        "INSERT INTO search_events \
             (id, kind, surface, subject_id, actor_user_id, position, had_query, sort, request_id) \
         SELECT * FROM unnest($1::uuid[], $2::text[], $3::text[], $4::uuid[], $5::uuid[], \
                              $6::int[], $7::bool[], $8::text[], $9::text[])",
    )
    .bind(&ids)
    .bind(&kinds)
    .bind(&surfaces)
    .bind(&subjects)
    .bind(&actors)
    .bind(&positions)
    .bind(&queried)
    .bind(&sorts)
    .bind(&requests)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected())
}

/// Click-through rate by position, which is the first thing to look at and the
/// last thing a golden set can tell you.
///
/// A ranking is working when this falls steeply with position: the top result
/// is opened far more than the tenth. Flat means the order is carrying no
/// information, whatever the golden set says about the set of results.
pub async fn rate_by_position(
    conn: &mut PgConnection,
    surface: Surface,
    days: i32,
) -> Result<Vec<(i32, i64, i64)>, AppError> {
    sqlx::query_as(
        "SELECT position, \
                count(*) FILTER (WHERE kind = 'impression') AS shown, \
                count(*) FILTER (WHERE kind = 'click') AS opened \
           FROM search_events \
          WHERE surface = $1 AND created_at > now() - make_interval(days => $2) \
          GROUP BY position \
          ORDER BY position",
    )
    .bind(surface.as_str())
    .bind(days)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire spelling and the database spelling are one string. Two
    /// hand-written lists would drift into a 500 rather than a compile error,
    /// which is why every other vocabulary here is pinned the same way.
    #[test]
    fn the_vocabularies_match_what_the_check_constraints_allow() {
        assert_eq!(
            Kind::ALL.map(Kind::as_str),
            ["impression", "click", "contact"]
        );
        assert_eq!(Surface::ALL.map(Surface::as_str), ["directory", "jobs"]);
    }
}
