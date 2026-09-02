//! The weekly job-alert pass.
//!
//! Run from a timer (`cm-server job-alerts` behind `cm-job-alerts.timer`),
//! not from the mail worker: the cadence is the product decision — weekly —
//! and a systemd timer is where this codebase already keeps such decisions
//! (`cm-verification.timer`, `cm-prune.timer`).
//!
//! Each batch is one transaction of pure SQL: claim unmatched jobs, reverse
//! match them against saved searches, render one digest per user into the
//! email outbox, mark everything. A crash is a clean rollback — no half-sent
//! state exists, because nothing here talks to a network. The mail worker
//! delivers what this pass enqueues.

use cm_core::{AppError, Origin, Secret};
use cm_db::repo::email_outbox::{self, Kind, NewEmail};
use cm_db::repo::reference;
use cm_db::repo::saved_searches::{self, AlertJob, AlertMatch};
use cm_db::PgPool;
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Jobs claimed per transaction.
pub const BATCH: i64 = 500;
/// A pass never runs longer than this many batches; a backlog beyond it waits
/// for the next timer firing rather than holding locks all day.
pub const MAX_BATCHES: usize = 40;
/// Jobs itemised per digest before "and N more".
const DIGEST_LINES: usize = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub jobs_considered: u64,
    pub jobs_matched: u64,
    pub digests: u64,
}

/// One full pass: loop until no unmatched jobs remain or the batch cap is hit.
pub async fn run(
    pool: &PgPool,
    pepper: &Secret<String>,
    site_origin: &Origin,
) -> Result<Stats, AppError> {
    let mut stats = Stats::default();

    for _ in 0..MAX_BATCHES {
        let batch = run_batch(pool, pepper, site_origin).await?;
        stats.jobs_considered += batch.jobs_considered;
        stats.jobs_matched += batch.jobs_matched;
        stats.digests += batch.digests;

        if batch.jobs_considered < BATCH as u64 {
            break;
        }
    }

    Ok(stats)
}

async fn run_batch(
    pool: &PgPool,
    pepper: &Secret<String>,
    site_origin: &Origin,
) -> Result<Stats, AppError> {
    let mut stats = Stats::default();

    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    let job_ids = saved_searches::claim_unmatched_jobs(&mut tx, BATCH).await?;
    stats.jobs_considered = job_ids.len() as u64;
    if job_ids.is_empty() {
        return Ok(stats);
    }

    let matches = saved_searches::matches_for_jobs(&mut tx, &job_ids).await?;

    if !matches.is_empty() {
        let matched_job_ids: Vec<Uuid> = {
            let mut ids: Vec<Uuid> = matches.iter().map(|m| m.job_id).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        stats.jobs_matched = matched_job_ids.len() as u64;

        let jobs: HashMap<Uuid, AlertJob> = saved_searches::alert_jobs(&mut tx, &matched_job_ids)
            .await?
            .into_iter()
            .map(|job| (job.id, job))
            .collect();

        // One digest per user, however many searches fired. BTreeMap so the
        // rendering order is stable for the tests and the logs.
        let mut per_user: BTreeMap<Uuid, Vec<&AlertMatch>> = BTreeMap::new();
        for alert in &matches {
            per_user.entry(alert.user_id).or_default().push(alert);
        }

        let mut notified_searches: Vec<Uuid> = Vec::new();
        for (user_id, alerts) in per_user {
            let email = render_digest(pepper, site_origin, &alerts, &jobs);
            email_outbox::enqueue(
                &mut tx,
                &NewEmail {
                    user_id,
                    recipient: alerts[0].email.clone(),
                    kind: Kind::JobAlert,
                    subject: email.subject,
                    body_text: email.body_text,
                    body_html: Some(email.body_html),
                    unsubscribe_url: Some(email.unsubscribe_url),
                },
            )
            .await?;
            stats.digests += 1;
            notified_searches.extend(alerts.iter().map(|alert| alert.search_id));
        }

        notified_searches.sort_unstable();
        notified_searches.dedup();
        saved_searches::touch_notified(&mut tx, &notified_searches).await?;
    }

    // Matched or not, considered is considered: nothing alerts twice.
    saved_searches::mark_jobs_matched(&mut tx, &job_ids).await?;
    tx.commit().await.map_err(AppError::internal)?;

    Ok(stats)
}

/// Saving is bucketed per account like posting is: a person curates a
/// handful of searches, and the row cap (`MAX_PER_USER`) bounds the total
/// while this bounds the churn.
fn saved_search_policy() -> cm_auth::ratelimit::Policy {
    cm_auth::ratelimit::Policy {
        name: "saved_search_create:user",
        limit: 30,
        window: chrono::Duration::days(1),
    }
}

/// Save a search from the same raw query string the live board takes.
///
/// Routed through `jobs::parse` and the same trade vocabularies as the board,
/// so what a saved row means is exactly what the URL it was saved from showed
/// — including the alias expansion of the query text, frozen at save time.
pub async fn create_saved_search(
    pool: &PgPool,
    pepper: &Secret<String>,
    user_id: Uuid,
    name: &str,
    raw: &crate::jobs::RawQuery,
) -> Result<saved_searches::SavedSearch, AppError> {
    cm_auth::ratelimit::enforce(
        pool,
        pepper,
        saved_search_policy(),
        &user_id.to_string(),
        chrono::Utc::now(),
    )
    .await?;

    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(AppError::invalid(
            "Give the search a name, up to 120 characters.",
        ));
    }

    let mut conn = pool.acquire().await.map_err(AppError::internal)?;

    let trade_ids = match raw
        .trade
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(list) => {
            let slugs: Vec<String> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            reference::trade_ids_for_slugs(&mut conn, &slugs).await?
        }
        None => Vec::new(),
    };

    let mut query = crate::jobs::parse(raw, trade_ids)?;
    if let Some(text) = query.filters.query.as_deref() {
        query.filters.query_trade_ids = reference::trades_matching_text(&mut conn, text).await?;
    }

    saved_searches::create(&mut conn, user_id, name, &query.filters).await
}

struct Digest {
    subject: String,
    body_text: String,
    body_html: String,
    unsubscribe_url: String,
}

/// The unsubscribe URL for one saved search, on the site's own pages.
pub fn unsubscribe_url(pepper: &Secret<String>, site_origin: &Origin, search_id: Uuid) -> String {
    format!(
        "{}/unsubscribe?search={search_id}&token={}",
        site_origin.as_str(),
        cm_auth::hash::unsubscribe_token(pepper, &search_id.to_string()),
    )
}

fn render_digest(
    pepper: &Secret<String>,
    site_origin: &Origin,
    alerts: &[&AlertMatch],
    jobs: &HashMap<Uuid, AlertJob>,
) -> Digest {
    // Distinct jobs, in match order, each listed once even if several of the
    // user's searches caught it.
    let mut seen: Vec<Uuid> = Vec::new();
    for alert in alerts {
        if !seen.contains(&alert.job_id) {
            seen.push(alert.job_id);
        }
    }

    let total = seen.len();
    let subject = if total == 1 {
        "1 new job matches your saved search".to_owned()
    } else {
        format!("{total} new jobs match your saved searches")
    };

    let mut body = String::from("New on the job board this week:\n");
    let mut cards = String::new();
    for job_id in seen.iter().take(DIGEST_LINES) {
        let Some(job) = jobs.get(job_id) else {
            continue;
        };
        let facts = format!(
            "{trade}, ZIP {zip}, {timeline}{budget}",
            trade = job.trade.as_deref().unwrap_or("Other"),
            zip = job.postal_code,
            timeline = timeline_words(&job.timeline),
            budget = budget_words(job.budget_min_cents, job.budget_max_cents),
        );
        let url = format!("{}/jobs/{}", site_origin.as_str(), job.id);

        body.push_str(&format!(
            "\n  • {title} — {facts}\n    {url}\n",
            title = job.title,
        ));
        // Escaping happens inside digest_job: the title is the one field a
        // stranger typed, and it lands inside markup.
        cards.push_str(&cm_auth::mail::digest_job(&url, &job.title, &facts));
    }

    if total > DIGEST_LINES {
        body.push_str(&format!(
            "\n  …and {} more: {}/jobs\n",
            total - DIGEST_LINES,
            site_origin.as_str()
        ));
        cards.push_str(&cm_auth::mail::paragraph(&format!(
            "…and {count} more on {link}.",
            count = total - DIGEST_LINES,
            link = cm_auth::mail::link(&format!("{}/jobs", site_origin.as_str()), "the job board"),
        )));
    }

    // The footer names each search that fired and how to stop it. The header
    // one-click URL is the first search's — one message can carry one.
    let mut footer_searches: Vec<(&str, Uuid)> = Vec::new();
    for alert in alerts {
        if !footer_searches.iter().any(|(_, id)| *id == alert.search_id) {
            footer_searches.push((&alert.search_name, alert.search_id));
        }
    }
    body.push_str("\nYou get this email because of your saved searches:\n");
    let mut footer_html = String::from("You get this email because of your saved searches:<br>");
    for (name, id) in &footer_searches {
        let stop = unsubscribe_url(pepper, site_origin, *id);
        body.push_str(&format!("  {name} — stop: {stop}\n"));
        // The name is the person's own words, and still escaped: their own
        // inbox is no safer a place to render unescaped markup than anyone
        // else's, and a name is copied from a form that accepts anything.
        footer_html.push_str(&format!(
            "<br><strong style=\"color:#4c5a75;\">{name}</strong> — \
             <a href=\"{stop}\" style=\"color:#6b7688;\">stop these</a>",
            name = cm_auth::mail::escape(name),
        ));
    }

    let html_body = format!(
        "{label}{heading}{lead}{cards}{footer}",
        label = cm_auth::mail::label("Job board"),
        heading = cm_auth::mail::heading(if total == 1 {
            "A new job matches your saved search"
        } else {
            "New jobs match your saved searches"
        }),
        lead = cm_auth::mail::paragraph("Posted on the job board since your last digest."),
        footer = cm_auth::mail::footnote(&footer_html),
    );

    Digest {
        subject,
        body_text: body,
        body_html: cm_auth::mail::shell(
            &format!(
                "{total} new {} on the job board",
                if total == 1 { "job" } else { "jobs" }
            ),
            &html_body,
        ),
        unsubscribe_url: unsubscribe_url(pepper, site_origin, footer_searches[0].1),
    }
}

fn timeline_words(timeline: &str) -> &'static str {
    match timeline {
        "asap" => "wanted ASAP",
        "within_2_weeks" => "within 2 weeks",
        "more_than_2_weeks" => "more than 2 weeks out",
        _ => "timing flexible",
    }
}

fn budget_words(min: Option<i64>, max: Option<i64>) -> String {
    match (min, max) {
        (Some(min), Some(max)) => {
            format!(", budget ${}–${}", min / 100, max / 100)
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_words_cover_the_vocabulary() {
        for value in ["asap", "within_2_weeks", "more_than_2_weeks", "unsure"] {
            assert!(!timeline_words(value).is_empty());
        }
    }

    #[test]
    fn a_missing_budget_says_nothing_rather_than_zero() {
        assert_eq!(budget_words(None, None), "");
        assert_eq!(
            budget_words(Some(100_000), Some(500_000)),
            ", budget $1000–$5000"
        );
    }
}
