//! Saved searches and the weekly job-alert digests.

mod common;

use cm_core::{Origin, Secret};
use common::{router, seed_directory, Client, PEPPER, SITE_ORIGIN};
use http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

const HOMEOWNER: &str = "poster@example.test";
const CONTRACTOR: &str = "pro@example.test";

fn pepper() -> Secret<String> {
    Secret::new(PEPPER.to_owned())
}

fn origin() -> Origin {
    Origin::parse(SITE_ORIGIN).expect("origin")
}

/// Post one job through the real endpoint, with overrides on the full fixture.
async fn post_job(client: &mut Client, overrides: serde_json::Value) -> Uuid {
    let mut body = json!({
        "title": "Rewire a 1920s duplex",
        "description": "Knob and tube throughout. Needs a full rewire and a new panel.",
        "trade": "electrician",
        "build_type": "replacement",
        "job_size": "Whole house, roughly 1,800 sq ft",
        "postal_code": "90026",
        "budget": { "min_cents": 800_000, "max_cents": 1_500_000 },
        "timeline": "within_2_weeks"
    });
    for (key, value) in overrides.as_object().expect("overrides").clone() {
        body[key] = value;
    }

    let response = client.post("/v1/jobs", body).await;
    assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.json);
    response.json["id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("uuid")
}

async fn save_search(client: &mut Client, body: serde_json::Value) -> common::TestResponse {
    client.post("/v1/saved-searches", body).await
}

async fn run_alerts(pool: &PgPool) -> cm_domain::job_alerts::Stats {
    cm_domain::job_alerts::run(pool, &pepper(), &origin())
        .await
        .expect("alert pass")
}

/// The digest a user was sent, as both bodies.
async fn digest_bodies(pool: &PgPool, email: &str) -> (String, String) {
    sqlx::query_as(
        "SELECT body_text, coalesce(body_html, '') FROM email_outbox \
          WHERE kind = 'job_alert' AND recipient = $1 ORDER BY created_at LIMIT 1",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .expect("a digest")
}

async fn digests_for(pool: &PgPool, email: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT body_text, unsubscribe_url FROM email_outbox \
          WHERE kind = 'job_alert' AND recipient = $1 ORDER BY created_at",
    )
    .bind(email)
    .fetch_all(pool)
    .await
    .expect("outbox")
}

/// The drift guard: for several filter shapes, the set of jobs the live board
/// returns for a query must equal the set of jobs the reverse match hands the
/// saved search built from the same query. If a clause changes on one side
/// and not the other, this is the test that goes red.
#[sqlx::test(migrations = "../../migrations")]
async fn a_saved_search_matches_exactly_what_the_live_board_returns(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut poster = Client::new(router.clone());
    poster.register(HOMEOWNER).await;

    // A varied board: two trades, two ZIPs, a budget spread, an unsure budget,
    // and a title the alias vocabulary maps to plumbing.
    post_job(&mut poster, json!({})).await;
    post_job(
        &mut poster,
        json!({ "title": "Replace a water heater before it floods the garage",
                "trade": "plumber", "postal_code": "90401", "timeline": "asap" }),
    )
    .await;
    post_job(
        &mut poster,
        json!({ "trade": "plumber", "budget": "unsure", "build_type": "repair" }),
    )
    .await;
    post_job(
        &mut poster,
        json!({ "trade": "other",
                "budget": { "min_cents": 5_000_000, "max_cents": 9_000_000 } }),
    )
    .await;

    let mut pro = Client::new(router);
    pro.register_contractor(CONTRACTOR).await;

    // (query-string parameters, as both a board query and a saved search)
    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("trade filter", json!({ "trade": "plumber" })),
        ("zip filter", json!({ "zip": "90401" })),
        ("alias query", json!({ "q": "water heater" })),
        ("text query", json!({ "q": "rewire" })),
        (
            "budget floor excludes unsure budgets",
            json!({ "budget_min": "1000000" }),
        ),
        ("timeline", json!({ "timeline": "asap" })),
        ("build type", json!({ "build_type": "repair" })),
        (
            "radius around Silver Lake",
            json!({ "lat": "34.0868", "lon": "-118.2702", "radius_m": "3000" }),
        ),
        (
            "combined",
            json!({ "trade": "plumber", "timeline": "asap", "zip": "90401" }),
        ),
    ];

    for (label, params) in cases {
        let query_string: String = params
            .as_object()
            .expect("params")
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str().expect("string").replace(' ', "%20")))
            .collect::<Vec<_>>()
            .join("&");

        let board = pro.get(&format!("/v1/jobs?{query_string}")).await;
        assert_eq!(board.status, StatusCode::OK, "{label}");
        let mut board_ids: Vec<String> = board.json["jobs"]
            .as_array()
            .expect("jobs")
            .iter()
            .map(|job| job["id"].as_str().expect("id").to_owned())
            .collect();
        board_ids.sort();

        let mut body = params.clone();
        body["name"] = json!(label);
        let saved = save_search(&mut pro, body).await;
        assert_eq!(
            saved.status,
            StatusCode::CREATED,
            "{label}: {:?}",
            saved.json
        );
        let search_id: Uuid = saved.json["id"]
            .as_str()
            .expect("id")
            .parse()
            .expect("uuid");

        let all_jobs: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM jobs")
            .fetch_all(&pool)
            .await
            .expect("jobs");
        let mut conn = pool.acquire().await.expect("connection");
        let mut matched_ids: Vec<String> =
            cm_db::repo::saved_searches::matches_for_jobs(&mut conn, &all_jobs)
                .await
                .expect("reverse match")
                .into_iter()
                .filter(|m| m.search_id == search_id)
                .map(|m| m.job_id.to_string())
                .collect();
        matched_ids.sort();

        assert_eq!(
            board_ids, matched_ids,
            "{label}: the board and the reverse match disagree"
        );
    }
}

/// One user, several firing searches, many jobs: exactly one email, each job
/// listed once, with the one-click unsubscribe attached.
#[sqlx::test(migrations = "../../migrations")]
async fn one_digest_email_carries_every_match_for_a_user(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut poster = Client::new(router.clone());
    poster.register(HOMEOWNER).await;
    let electric = post_job(&mut poster, json!({})).await;
    let plumbing = post_job(
        &mut poster,
        json!({ "title": "Water heater is done for, needs replacing this week",
                "trade": "plumber", "timeline": "asap" }),
    )
    .await;

    let mut pro = Client::new(router);
    pro.register_contractor(CONTRACTOR).await;
    save_search(
        &mut pro,
        json!({ "name": "Everything electric", "trade": "electrician" }),
    )
    .await;
    save_search(
        &mut pro,
        json!({ "name": "Anything urgent", "timeline": "asap" }),
    )
    .await;
    save_search(&mut pro, json!({ "name": "All of it" })).await;

    let stats = run_alerts(&pool).await;
    assert_eq!(stats.digests, 1, "{stats:?}");
    assert_eq!(stats.jobs_matched, 2, "{stats:?}");

    let digests = digests_for(&pool, CONTRACTOR).await;
    assert_eq!(digests.len(), 1);
    let (body, unsubscribe) = &digests[0];
    assert_eq!(
        body.matches(&electric.to_string()).count(),
        1,
        "each job appears exactly once: {body}"
    );
    assert_eq!(body.matches(&plumbing.to_string()).count(), 1, "{body}");
    assert!(
        body.contains("Everything electric") && body.contains("Anything urgent"),
        "the footer names the searches that fired: {body}"
    );
    assert!(
        unsubscribe
            .as_deref()
            .is_some_and(|u| u.contains("/unsubscribe?search=")),
        "the one-click header URL rides the outbox row: {unsubscribe:?}"
    );
}

/// The digest goes out as both bodies, and the HTML one is a real document.
///
/// A message with no text part scores worse with spam filters and is what a
/// screen reader reads, so "we added HTML" must never have meant "we dropped
/// text".
#[sqlx::test(migrations = "../../migrations")]
async fn the_digest_carries_a_text_body_and_an_html_document(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut poster = Client::new(router.clone());
    poster.register(HOMEOWNER).await;
    post_job(&mut poster, json!({})).await;

    let mut pro = Client::new(router);
    pro.register_contractor(CONTRACTOR).await;
    save_search(&mut pro, json!({ "name": "Wide net" })).await;
    run_alerts(&pool).await;

    let (text, html) = digest_bodies(&pool, CONTRACTOR).await;

    assert!(text.contains("Rewire a 1920s duplex"), "{text}");
    assert!(
        !text.contains('<'),
        "the text part must not be markup: {text}"
    );

    assert!(html.starts_with("<!doctype html>"), "{html}");
    assert!(html.contains("Rewire a 1920s duplex"), "{html}");
    assert!(
        html.contains("Wide net"),
        "the footer names the search: {html}"
    );
    assert!(html.contains("/unsubscribe?search="), "{html}");
    // Layout that survives Outlook, and no remote asset to be blocked.
    assert!(html.contains("role=\"presentation\""), "{html}");
    assert!(!html.contains("<img"), "no remote images: {html}");
}

/// A job title is typed by a stranger and lands in somebody else's inbox as
/// markup. This is the end-to-end proof that it cannot break out of it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_hostile_job_title_is_escaped_in_the_digest(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut poster = Client::new(router.clone());
    poster.register(HOMEOWNER).await;
    post_job(
        &mut poster,
        json!({ "title": "Rewire </a><script>alert(1)</script><a href=\"https://evil.test\">" }),
    )
    .await;

    let mut pro = Client::new(router);
    pro.register_contractor(CONTRACTOR).await;
    save_search(&mut pro, json!({ "name": "Wide net" })).await;
    run_alerts(&pool).await;

    let (_, html) = digest_bodies(&pool, CONTRACTOR).await;

    assert!(!html.contains("<script>"), "{html}");
    assert!(
        !html.contains("href=\"https://evil.test\""),
        "an injected anchor reached the inbox: {html}"
    );
    assert!(html.contains("&lt;script&gt;"), "{html}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn your_own_job_never_alerts_you(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut poster = Client::new(router);
    poster.register(HOMEOWNER).await;
    save_search(&mut poster, json!({ "name": "My own wide net" })).await;
    post_job(&mut poster, json!({})).await;

    let stats = run_alerts(&pool).await;
    assert_eq!(stats.digests, 0, "{stats:?}");
    assert!(digests_for(&pool, HOMEOWNER).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_unsubscribed_search_stops_matching_but_keeps_its_row(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut poster = Client::new(router.clone());
    poster.register(HOMEOWNER).await;
    post_job(&mut poster, json!({})).await;

    let mut pro = Client::new(router.clone());
    pro.register_contractor(CONTRACTOR).await;
    let saved = save_search(&mut pro, json!({ "name": "Wide net" })).await;
    let search_id = saved.json["id"].as_str().expect("id").to_owned();

    // The one-click POST, exactly as a mail client would send it: no session.
    let token = cm_auth::hash::unsubscribe_token(&pepper(), &search_id);
    let mut anonymous = Client::new(router);
    let response = anonymous
        .post(
            &format!("/v1/saved-searches/{search_id}/unsubscribe?token={token}"),
            json!({}),
        )
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let stats = run_alerts(&pool).await;
    assert_eq!(stats.digests, 0, "{stats:?}");

    let list = pro.get("/v1/saved-searches").await;
    assert_eq!(
        list.json[0]["notify"], false,
        "the row survives: {:?}",
        list.json
    );

    // Idempotent: the same link again is still a 204.
    let again = anonymous
        .post(
            &format!("/v1/saved-searches/{search_id}/unsubscribe?token={token}"),
            json!({}),
        )
        .await;
    assert_eq!(again.status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_forged_unsubscribe_token_is_refused(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut pro = Client::new(router.clone());
    pro.register_contractor(CONTRACTOR).await;
    let saved = save_search(&mut pro, json!({ "name": "Wide net" })).await;
    let search_id = saved.json["id"].as_str().expect("id").to_owned();

    let mut anonymous = Client::new(router);
    let response = anonymous
        .post(
            &format!("/v1/saved-searches/{search_id}/unsubscribe?token=not-the-real-token"),
            json!({}),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    let list = pro.get("/v1/saved-searches").await;
    assert_eq!(list.json[0]["notify"], true, "{:?}", list.json);
}

/// Closed before the pass runs: considered, marked, but nobody is emailed.
#[sqlx::test(migrations = "../../migrations")]
async fn a_closed_job_is_marked_matched_but_alerts_nobody(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut poster = Client::new(router.clone());
    poster.register(HOMEOWNER).await;
    let job = post_job(&mut poster, json!({})).await;
    poster
        .post(&format!("/v1/jobs/{job}/close"), json!({}))
        .await;

    let mut pro = Client::new(router);
    pro.register_contractor(CONTRACTOR).await;
    save_search(&mut pro, json!({ "name": "Wide net" })).await;

    let stats = run_alerts(&pool).await;
    assert_eq!(stats.jobs_considered, 1, "{stats:?}");
    assert_eq!(stats.digests, 0, "{stats:?}");

    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE alerts_matched_at IS NULL")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(pending, 0, "considered is considered, matched or not");
}

/// The pass is idempotent across runs: what alerted once never alerts again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_second_alert_run_sends_nothing_new(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let mut poster = Client::new(router.clone());
    poster.register(HOMEOWNER).await;
    post_job(&mut poster, json!({})).await;

    let mut pro = Client::new(router);
    pro.register_contractor(CONTRACTOR).await;
    save_search(&mut pro, json!({ "name": "Wide net" })).await;

    let first = run_alerts(&pool).await;
    assert_eq!(first.digests, 1);

    let second = run_alerts(&pool).await;
    assert_eq!(second.digests, 0, "{second:?}");
    assert_eq!(second.jobs_considered, 0, "{second:?}");
    assert_eq!(digests_for(&pool, CONTRACTOR).await.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_saved_search_cap_is_enforced(pool: PgPool) {
    seed_directory(&pool).await;
    let mut pro = Client::new(router(pool));
    pro.register_contractor(CONTRACTOR).await;

    // The creation rate limit (30/day) sits above the row cap (20), so the cap
    // is what refuses the twenty-first.
    for n in 0..20 {
        let saved = save_search(&mut pro, json!({ "name": format!("Search {n}") })).await;
        assert_eq!(
            saved.status,
            StatusCode::CREATED,
            "search {n}: {:?}",
            saved.json
        );
    }

    let over = save_search(&mut pro, json!({ "name": "One too many" })).await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST);
    assert!(
        over.json["error"]["message"]
            .as_str()
            .expect("message")
            .contains("20"),
        "{:?}",
        over.json
    );
}

/// Deleting is owner-scoped: someone else's search is a 404, not a deletion.
#[sqlx::test(migrations = "../../migrations")]
async fn deleting_a_saved_search_is_owner_scoped(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool);
    let mut owner = Client::new(router.clone());
    owner.register_contractor(CONTRACTOR).await;
    let saved = save_search(&mut owner, json!({ "name": "Mine" })).await;
    let id = saved.json["id"].as_str().expect("id").to_owned();

    let mut other = Client::new(router);
    other.register("someone-else@example.test").await;
    let stolen = other.delete(&format!("/v1/saved-searches/{id}")).await;
    assert_eq!(stolen.status, StatusCode::NOT_FOUND);

    let deleted = owner.delete(&format!("/v1/saved-searches/{id}")).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let list = owner.get("/v1/saved-searches").await;
    assert_eq!(list.json.as_array().expect("list").len(), 0);
}
