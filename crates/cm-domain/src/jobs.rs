//! Posting a job, and the rules for browsing them.
//!
//! Two things live here that the layers either side deliberately do not: the
//! transaction boundary around a post (job row, ZIP centroid and audit row
//! commit together or not at all), and the query parser that decides which
//! filters are forgiving and which are fatal.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use cm_auth::ratelimit;
use cm_core::{new_id, AppError, Secret};
use cm_db::repo::audit::{self, ActorKind, AuditEvent};
use cm_db::repo::jobs::{
    self, Cursor, Filters, JobStatus, JobTimeline, NewJob, Near, OwnerJob, DEFAULT_PAGE, MAX_PAGE,
};
use cm_db::repo::reference;
use cm_db::PgPool;
use uuid::Uuid;

/// Posting is infrequent for a real person and creates content other people
/// read, so it is bucketed per account rather than per address — the same shape
/// as opening a conversation.
fn job_post_policy() -> ratelimit::Policy {
    ratelimit::Policy {
        name: "job_post:user",
        limit: 10,
        window: chrono::Duration::days(1),
    }
}

pub struct PostJob {
    pub title: String,
    pub description: String,
    pub trade_slug: Option<String>,
    pub postal_code: Option<String>,
    pub budget_min_cents: Option<i64>,
    pub budget_max_cents: Option<i64>,
    pub timeline: Option<JobTimeline>,
}

/// Post a job.
///
/// The account-type rule (homeowners only) is checked in the handler so the
/// caller gets a 403 with an explanation; the database trigger behind this is
/// the backstop for a path that forgets.
pub async fn post(
    pool: &PgPool,
    pepper: &Secret<String>,
    poster: Uuid,
    input: PostJob,
    request_id: Option<String>,
) -> Result<OwnerJob, AppError> {
    // Outside the transaction, and deliberately: a refused post should still
    // count against the limit.
    ratelimit::enforce(
        pool,
        pepper,
        job_post_policy(),
        &poster.to_string(),
        Utc::now(),
    )
    .await?;

    let title = input.title.trim();
    let description = input.description.trim();
    if title.is_empty() {
        return Err(AppError::invalid("A title is required."));
    }
    if title.chars().count() > 140 {
        return Err(AppError::invalid("A title must be under 140 characters."));
    }
    if description.is_empty() {
        return Err(AppError::invalid("A description is required."));
    }
    if description.chars().count() > 4000 {
        return Err(AppError::invalid(
            "A description must be under 4000 characters.",
        ));
    }
    if let (Some(min), Some(max)) = (input.budget_min_cents, input.budget_max_cents) {
        if min > max {
            return Err(AppError::invalid(
                "The lower end of the budget must not exceed the upper end.",
            ));
        }
    }
    if input.budget_min_cents.is_some_and(|c| c < 0) || input.budget_max_cents.is_some_and(|c| c < 0)
    {
        return Err(AppError::invalid("A budget cannot be negative."));
    }

    let postal_code = match input.postal_code.as_deref().map(str::trim) {
        Some("") | None => None,
        Some(zip) if zip.len() == 5 && zip.chars().all(|c| c.is_ascii_digit()) => Some(zip),
        Some(_) => return Err(AppError::invalid("Enter a valid five-digit ZIP code.")),
    };

    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    let trade_id = match input.trade_slug.as_deref().map(str::trim) {
        Some(slug) if !slug.is_empty() => {
            let ids = reference::trade_ids_for_slugs(&mut tx, &[slug.to_owned()]).await?;
            match ids.first() {
                Some(id) => Some(*id),
                None => return Err(AppError::invalid(format!("Unknown trade \"{slug}\"."))),
            }
        }
        _ => None,
    };

    // The published location, decided the same way a contractor's is: the ZIP
    // centroid, or nothing. A job with an unknown ZIP is unlocated rather than
    // dropped at a plausible-looking point.
    let region = match postal_code {
        Some(code) => reference::find_zcta(&mut tx, code).await?,
        None => None,
    };

    let id = new_id();
    jobs::insert(
        &mut tx,
        NewJob {
            id,
            posted_by_user_id: poster,
            title,
            description,
            trade_id,
            budget_min_cents: input.budget_min_cents,
            budget_max_cents: input.budget_max_cents,
            timeline: input.timeline,
            postal_code,
            region_id: region.as_ref().map(|r| r.id),
            centroid: region.as_ref().map(|r| (r.lon, r.lat)),
        },
    )
    .await?;

    audit::record(
        &mut tx,
        AuditEvent::new("job.posted", "jobs")
            .actor(ActorKind::User, Some(poster))
            .subject(id)
            .data(serde_json::json!({
                "trade_id": trade_id,
                "postal_code": postal_code,
                "located": region.is_some(),
            }))
            .request_id(request_id),
    )
    .await?;

    let posted = jobs::for_poster(&mut tx, poster)
        .await?
        .into_iter()
        .find(|job| job.public.id == id)
        .ok_or_else(|| AppError::internal("the job was inserted but could not be read back"))?;

    tx.commit().await.map_err(AppError::internal)?;
    Ok(posted)
}

/// Close or cancel a job.
///
/// A job that belongs to somebody else answers 404 rather than 403 — the same
/// rule claims use, because "that is not yours" already tells a stranger the id
/// is real.
pub async fn close(
    pool: &PgPool,
    poster: Uuid,
    job_id: Uuid,
    status: JobStatus,
    request_id: Option<String>,
) -> Result<(), AppError> {
    if status == JobStatus::Open {
        return Err(AppError::invalid("A job can only be closed or cancelled."));
    }

    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    match jobs::poster_of(&mut tx, job_id).await? {
        Some(owner) if owner == poster => {}
        _ => return Err(AppError::NotFound),
    }

    if !jobs::close(&mut tx, job_id, poster, status).await? {
        return Err(AppError::conflict("That job is no longer open."));
    }

    audit::record(
        &mut tx,
        AuditEvent::new("job.closed", "jobs")
            .actor(ActorKind::User, Some(poster))
            .subject(job_id)
            .data(serde_json::json!({ "status": status.as_str() }))
            .request_id(request_id),
    )
    .await?;

    tx.commit().await.map_err(AppError::internal)?;
    Ok(())
}

/* ── Query parsing ─────────────────────────────────────────────────────────
 *
 * The same rule the directory uses: a junk optional filter is dropped and
 * named, because a shared link with one bad parameter should still show
 * results. A junk cursor or page size is fatal, because silently returning the
 * wrong page looks like data loss.
 */

#[derive(Debug, Default, serde::Deserialize)]
pub struct RawQuery {
    pub trade: Option<String>,
    pub zip: Option<String>,
    pub lat: Option<String>,
    pub lon: Option<String>,
    pub radius_m: Option<String>,
    pub limit: Option<String>,
    pub cursor: Option<String>,
}

pub struct JobQuery {
    pub filters: Filters,
    pub limit: i64,
    pub cursor: Option<Cursor>,
    pub ignored: Vec<String>,
}

/// Metres. Matches the directory's ceiling so the two searches feel the same.
const MAX_RADIUS_M: f64 = 200_000.0;
const DEFAULT_RADIUS_M: f64 = 25_000.0;

pub fn parse(raw: &RawQuery, trade_ids: Vec<Uuid>) -> Result<JobQuery, AppError> {
    let mut filters = Filters::default();
    let mut ignored = Vec::new();

    if !trade_ids.is_empty() {
        filters.trade_ids = Some(trade_ids);
    }

    match raw.zip.as_deref().map(str::trim).filter(|z| !z.is_empty()) {
        Some(zip) if zip.len() == 5 && zip.chars().all(|c| c.is_ascii_digit()) => {
            filters.postal_code = Some(zip.to_owned());
        }
        Some(_) => ignored.push("zip".to_owned()),
        None => {}
    }

    // Latitude, longitude and radius are one filter in three parts. A partial
    // set is a half-filled form, not an instruction to hide the county.
    match (
        parse_f64(raw.lat.as_deref()),
        parse_f64(raw.lon.as_deref()),
        parse_f64(raw.radius_m.as_deref()),
    ) {
        (Some(lat), Some(lon), radius)
            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) =>
        {
            filters.near = Some(Near {
                lat,
                lon,
                radius_m: radius.unwrap_or(DEFAULT_RADIUS_M).clamp(1.0, MAX_RADIUS_M),
            });
        }
        (None, None, None) => {}
        _ => ignored.push("lat/lon/radius_m".to_owned()),
    }

    let limit = match raw.limit.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
        Some(value) => value
            .parse::<i64>()
            .map_err(|_| AppError::invalid("limit must be a number"))?
            .clamp(1, MAX_PAGE),
        None => DEFAULT_PAGE,
    };

    let cursor = match raw.cursor.as_deref().filter(|c| !c.is_empty()) {
        Some(value) => Some(decode_cursor(value)?),
        None => None,
    };

    Ok(JobQuery {
        filters,
        limit,
        cursor,
        ignored,
    })
}

fn parse_f64(value: Option<&str>) -> Option<f64> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Opaque on purpose: a client that can read the sort key starts constructing
/// cursors, and then the encoding is a contract.
pub fn encode_cursor(cursor: &Cursor) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}\u{0}{}", cursor.id, cursor.created_at.to_rfc3339()))
}

pub fn decode_cursor(value: &str) -> Result<Cursor, AppError> {
    let invalid = || AppError::invalid("that page cursor is not valid");

    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    let text = String::from_utf8(decoded).map_err(|_| invalid())?;
    let (id, created_at) = text.split_once('\u{0}').ok_or_else(invalid)?;

    Ok(Cursor {
        id: Uuid::parse_str(id).map_err(|_| invalid())?,
        created_at: DateTime::parse_from_rfc3339(created_at)
            .map_err(|_| invalid())?
            .with_timezone(&Utc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_jobs_limit_is_bounded_and_distinct() {
        let policies = [job_post_policy()];
        for policy in &policies {
            assert!(policy.limit > 0, "{} has no limit", policy.name);
            assert!(
                policy.window > chrono::Duration::zero(),
                "{} has no window",
                policy.name
            );
        }
    }

    #[test]
    fn a_junk_optional_filter_is_dropped_rather_than_fatal() {
        let raw = RawQuery {
            zip: Some("banana".into()),
            lat: Some("not-a-number".into()),
            ..Default::default()
        };
        let parsed = parse(&raw, Vec::new()).expect("a bad optional filter must not fail the page");
        assert!(parsed.filters.postal_code.is_none());
        assert!(parsed.ignored.contains(&"zip".to_owned()));
    }

    #[test]
    fn a_partial_location_is_ignored_rather_than_half_applied() {
        let raw = RawQuery {
            lat: Some("34.1".into()),
            ..Default::default()
        };
        let parsed = parse(&raw, Vec::new()).expect("parse");
        assert!(parsed.filters.near.is_none());
        assert!(parsed.ignored.contains(&"lat/lon/radius_m".to_owned()));
    }

    #[test]
    fn a_junk_structural_parameter_is_fatal() {
        let raw = RawQuery {
            limit: Some("lots".into()),
            ..Default::default()
        };
        assert!(parse(&raw, Vec::new()).is_err(), "a bad limit must 400");

        let raw = RawQuery {
            cursor: Some("!!!not-base64!!!".into()),
            ..Default::default()
        };
        assert!(parse(&raw, Vec::new()).is_err(), "a bad cursor must 400");
    }

    #[test]
    fn a_cursor_round_trips() {
        let cursor = Cursor {
            id: new_id(),
            created_at: Utc::now(),
        };
        let decoded = decode_cursor(&encode_cursor(&cursor)).expect("round trip");
        assert_eq!(decoded.id, cursor.id);
        assert_eq!(
            decoded.created_at.timestamp_micros(),
            cursor.created_at.timestamp_micros()
        );
    }
}
