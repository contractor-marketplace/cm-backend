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
    self, BuildType, Cursor, Filters, JobStatus, JobTimeline, Near, NewJob, OwnerJob, DEFAULT_PAGE,
    MAX_PAGE,
};
use cm_db::repo::{job_photos, reference};
use cm_db::PgPool;
use cm_storage::Store;
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

/// A post, after the handler has parsed the vocabularies but before anything
/// has been checked against the database.
///
/// Every field is required. The escapes are carried as values, not as absent
/// fields: `trade_slug: None` means the poster picked "Other", and
/// `budget: None` means they picked "I'm not sure". The handler is what turns a
/// genuinely missing field into a 400 before it ever reaches here, which is
/// what lets `None` carry a meaning at all — see the header of
/// `migrations/0018_job_intake.sql`.
pub struct PostJob {
    pub title: String,
    pub description: String,
    /// `None` is the poster's "Other / not listed".
    pub trade_slug: Option<String>,
    pub build_type: BuildType,
    pub job_size: String,
    pub postal_code: String,
    /// `None` is the poster's "I'm not sure". Never half a range.
    pub budget: Option<(i64, i64)>,
    pub timeline: JobTimeline,
}

/// The shortest description that is worth a contractor's time to open.
///
/// Roughly one sentence. Not a quality bar — a floor under "new panel" as an
/// entire brief.
pub const MIN_DESCRIPTION: usize = 50;

/// Photos are bucketed per account like posting is, and more generously:
/// eight photos on each of ten jobs is eighty in a day, so the limit sits above
/// legitimate use and well below anything that would fill a bucket.
fn photo_upload_policy() -> ratelimit::Policy {
    ratelimit::Policy {
        name: "job_photo:user",
        limit: 100,
        window: chrono::Duration::days(1),
    }
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
    // Counted in characters, like the maximum, so a description in a language
    // that needs more bytes per character is not held to a longer standard.
    let described = description.chars().count();
    if described < MIN_DESCRIPTION {
        return Err(AppError::invalid(format!(
            "Please describe the work in at least {MIN_DESCRIPTION} characters \
             — you have written {described}. What needs doing, and where in the \
             property?"
        )));
    }
    if described > 4000 {
        return Err(AppError::invalid(
            "A description must be under 4000 characters.",
        ));
    }

    let job_size = input.job_size.trim();
    if job_size.is_empty() {
        return Err(AppError::invalid(
            "Tell us roughly how big the job is. \"Not sure yet\" is a fine answer.",
        ));
    }
    if job_size.chars().count() > 200 {
        return Err(AppError::invalid(
            "Keep the size to under 200 characters — the details belong in the description.",
        ));
    }

    if let Some((min, max)) = input.budget {
        if min < 0 || max < 0 {
            return Err(AppError::invalid("A budget cannot be negative."));
        }
        if min > max {
            return Err(AppError::invalid(
                "The lower end of the budget must not exceed the upper end.",
            ));
        }
    }

    let postal_code = input.postal_code.trim();
    if postal_code.len() != 5 || !postal_code.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::invalid("Enter a valid five-digit ZIP code."));
    }

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
    // An unknown ZIP still posts. It simply has no centroid, so the job is
    // listed but unmapped — better than refusing a real address because our
    // ZCTA import does not reach it.
    let region = reference::find_zcta(&mut tx, postal_code).await?;

    let id = new_id();
    jobs::insert(
        &mut tx,
        NewJob {
            id,
            posted_by_user_id: poster,
            title,
            description,
            trade_id,
            build_type: input.build_type,
            job_size,
            budget: input.budget,
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
                "build_type": input.build_type.as_str(),
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
    store: &Store,
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

    // Cancelling means take it down, so the objects go too. Closing does not:
    // the work happened, and the poster keeps their own record of it in
    // /v1/me/jobs. Read the keys before the rows are deleted — after that
    // there is nothing left to say which objects belonged to this job.
    let orphaned = if status == JobStatus::Cancelled {
        let keys = job_photos::keys_for_job(&mut tx, job_id).await?;
        job_photos::delete_all_for_job(&mut tx, job_id).await?;
        keys
    } else {
        Vec::new()
    };

    audit::record(
        &mut tx,
        AuditEvent::new("job.closed", "jobs")
            .actor(ActorKind::User, Some(poster))
            .subject(job_id)
            .data(serde_json::json!({
                "status": status.as_str(),
                "photos_removed": orphaned.len(),
            }))
            .request_id(request_id),
    )
    .await?;

    tx.commit().await.map_err(AppError::internal)?;

    // After the commit, deliberately. A storage failure must not roll back a
    // cancellation the poster asked for — the row is gone either way, so the
    // photo is unreachable through the product. What is left is an orphaned
    // object, which is logged loudly enough to sweep up.
    for key in orphaned {
        if let Err(error) = store.delete(&key).await {
            tracing::error!(%key, %job_id, ?error, "a cancelled job left an orphaned photo object");
        }
    }

    Ok(())
}

/// Attach a photo to a job.
///
/// The upload is normalised BEFORE anything is written: an unreadable file is a
/// 400 and no row, no object and no wasted transaction. The object is written
/// before the row so a failure leaves an orphaned object rather than a row
/// pointing at nothing — a photo that 404s in the page is worse than a few
/// bytes nobody references.
pub async fn attach_photo(
    pool: &PgPool,
    store: &Store,
    pepper: &Secret<String>,
    poster: Uuid,
    job_id: Uuid,
    bytes: &[u8],
    request_id: Option<String>,
) -> Result<jobs::Photo, AppError> {
    ratelimit::enforce(
        pool,
        pepper,
        photo_upload_policy(),
        &poster.to_string(),
        Utc::now(),
    )
    .await?;

    let mut conn = pool.acquire().await.map_err(AppError::internal)?;

    // Ownership first, and a 404 rather than a 403: "that is not yours" already
    // confirms the id is real.
    match jobs::poster_of(&mut conn, job_id).await? {
        Some(owner) if owner == poster => {}
        _ => return Err(AppError::NotFound),
    }

    let existing = job_photos::count_for_job(&mut conn, job_id).await?;
    if existing >= job_photos::MAX_PER_JOB {
        return Err(AppError::invalid(format!(
            "A job can have up to {} photos.",
            job_photos::MAX_PER_JOB
        )));
    }

    // The pass that makes the file safe to publish: it strips the EXIF a phone
    // writes into a photograph of a house, which would otherwise hand back the
    // exact address this schema was built to never hold.
    let normalised = cm_storage::normalise(bytes)?;

    let id = new_id();
    let key = cm_storage::photo_key(job_id, id);
    let url = store.put(&key, &normalised).await?;

    let row = job_photos::insert(
        &mut conn,
        job_photos::NewPhoto {
            id,
            job_id,
            storage_key: &key,
            byte_size: normalised.bytes.len() as i64,
            width: normalised.width as i32,
            height: normalised.height as i32,
        },
    )
    .await
    .inspect_err(|_| {
        tracing::error!(%key, %job_id, "a photo object was stored but its row was not");
    })?;

    audit::record(
        &mut conn,
        AuditEvent::new("job.photo_added", "job_photos")
            .actor(ActorKind::User, Some(poster))
            .subject(id)
            .data(serde_json::json!({ "job_id": job_id, "bytes": normalised.bytes.len() }))
            .request_id(request_id),
    )
    .await?;

    Ok(jobs::Photo {
        id: row.id,
        url,
        width: row.width,
        height: row.height,
    })
}

/// Remove a photo. The row goes first, then the object.
pub async fn remove_photo(
    pool: &PgPool,
    store: &Store,
    poster: Uuid,
    job_id: Uuid,
    photo_id: Uuid,
    request_id: Option<String>,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    match jobs::poster_of(&mut tx, job_id).await? {
        Some(owner) if owner == poster => {}
        _ => return Err(AppError::NotFound),
    }

    let key = job_photos::delete(&mut tx, job_id, photo_id)
        .await?
        .ok_or(AppError::NotFound)?;

    audit::record(
        &mut tx,
        AuditEvent::new("job.photo_removed", "job_photos")
            .actor(ActorKind::User, Some(poster))
            .subject(photo_id)
            .data(serde_json::json!({ "job_id": job_id }))
            .request_id(request_id),
    )
    .await?;

    tx.commit().await.map_err(AppError::internal)?;

    // After the commit, for the same reason as cancelling: the poster asked for
    // it gone, and it is gone from the product whether or not storage agrees.
    if let Err(error) = store.delete(&key).await {
        tracing::error!(%key, %photo_id, ?error, "a removed photo left an orphaned object");
    }

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

    let limit = match raw
        .limit
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
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
    URL_SAFE_NO_PAD.encode(format!(
        "{}\u{0}{}",
        cursor.id,
        cursor.created_at.to_rfc3339()
    ))
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
