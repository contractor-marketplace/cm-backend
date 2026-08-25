//! Posting a job, and the three tiers that browse it.
//!
//! The tests that matter most here are the negative ones: what an anonymous
//! caller cannot obtain, and what a contractor account cannot do.

mod common;

use common::{router, seed_directory, seed_jobs, user_id, Client};
use http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

/// A complete post. Silver Lake is one of the seeded ZCTAs, so it resolves to a
/// centroid and the job lands on the map.
fn a_job() -> serde_json::Value {
    json!({
        "title": "Rewire a 1920s duplex",
        "description": "Knob and tube throughout. Needs a full rewire and a new panel.",
        "trade": "electrician",
        "postal_code": "90026",
        "budget_min_cents": 800_000,
        "budget_max_cents": 1_500_000,
        "timeline": "within_a_month"
    })
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_homeowner_posts_and_a_contractor_sees_the_detail(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    let posted = homeowner.post("/v1/jobs", a_job()).await;
    assert_eq!(posted.status, StatusCode::CREATED, "{:?}", posted.json);
    assert_eq!(posted.json["title"], "Rewire a 1920s duplex");
    assert_eq!(posted.json["trade"], "electrician");
    assert_eq!(posted.json["location_precision"], "zip_centroid");
    assert!(posted.json["lat"].is_number(), "a known ZIP places the job");

    let mut contractor = Client::new(router.clone());
    contractor.register_contractor("sparks@example.test").await;
    let board = contractor.get("/v1/jobs").await;
    assert_eq!(board.status, StatusCode::OK, "{:?}", board.json);
    assert_eq!(board.json["detail_visible"], true);

    let first = &board.json["jobs"][0];
    assert_eq!(first["title"], "Rewire a 1920s duplex");
    assert!(
        first["description"].as_str().unwrap().contains("Knob and tube"),
        "a contractor sees the description"
    );
    assert_eq!(
        first["poster_first_name"], "Test",
        "a first name, from the display name 'Test Person'"
    );
}

/// The headline privacy assertion. An anonymous caller may browse, and may not
/// learn anything about the person who posted.
#[sqlx::test(migrations = "../../migrations")]
async fn an_anonymous_caller_gets_the_board_without_any_pii(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    let posted = homeowner.post("/v1/jobs", a_job()).await;
    let job_id = posted.json["id"].as_str().expect("id").to_owned();

    let mut anonymous = Client::new(router.clone());

    for path in [
        "/v1/jobs".to_owned(),
        format!("/v1/jobs/{job_id}"),
        // A filter is not a way in either: the shape must not change because
        // the caller asked a narrower question.
        "/v1/jobs?trade=electrician".to_owned(),
        "/v1/jobs?zip=90026".to_owned(),
        "/v1/jobs?lat=34.0781&lon=-118.2606&radius_m=50000".to_owned(),
        "/v1/jobs?limit=50".to_owned(),
    ] {
        let response = anonymous.get(&path).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert_eq!(response.json["detail_visible"], false, "{path}");

        let rendered = response.json.to_string();
        for leaked in [
            "Knob and tube",   // the description
            "owner@example",   // the address
            "Test Person",     // the full display name
            "poster_first_name",
            "posted_by_user_id",
            "description",
        ] {
            assert!(
                !rendered.contains(leaked),
                "{path} leaked {leaked}: {rendered}"
            );
        }

        // The public facts are still there, or the board would be useless.
        assert!(rendered.contains("Rewire a 1920s duplex"), "{path}");
    }
}

/// A signed-in homeowner is not a contractor, and gets the same view as a
/// stranger. The extra detail is for the side of the market that acts on it.
#[sqlx::test(migrations = "../../migrations")]
async fn another_homeowner_does_not_get_the_contractor_view(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut poster = Client::new(router.clone());
    poster.register("owner@example.test").await;
    poster.post("/v1/jobs", a_job()).await;

    let mut nosy = Client::new(router.clone());
    nosy.register("nosy@example.test").await;

    let board = nosy.get("/v1/jobs").await;
    assert_eq!(board.json["detail_visible"], false);
    assert!(!board.json.to_string().contains("Knob and tube"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_contractor_account_cannot_post_a_job(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut contractor = Client::new(router.clone());
    contractor.register_contractor("sparks@example.test").await;

    let refused = contractor.post("/v1/jobs", a_job()).await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "posting work is the homeowner's side"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn posting_needs_a_session(pool: PgPool) {
    seed_directory(&pool).await;
    let mut anonymous = Client::new(router(pool.clone()));
    let refused = anonymous.post("/v1/jobs", a_job()).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_poster_sees_their_own_job_in_full(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    homeowner.post("/v1/jobs", a_job()).await;

    let mine = homeowner.get("/v1/me/jobs").await;
    assert_eq!(mine.status, StatusCode::OK);
    assert_eq!(mine.json.as_array().expect("array").len(), 1);
    assert!(mine.json[0]["description"].as_str().unwrap().contains("Knob"));
    assert!(mine.json[0]["posted_by_user_id"].is_string());

    // Somebody else's list is their own, not a way to read anyone's.
    let mut other = Client::new(router.clone());
    other.register("other@example.test").await;
    let theirs = other.get("/v1/me/jobs").await;
    assert_eq!(theirs.json.as_array().expect("array").len(), 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn only_the_poster_may_close_a_job(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    let posted = homeowner.post("/v1/jobs", a_job()).await;
    let id = posted.json["id"].as_str().expect("id").to_owned();

    // Someone else's job is a 404, not a 403: "that is not yours" already
    // confirms the id is real.
    let mut stranger = Client::new(router.clone());
    stranger.register("stranger@example.test").await;
    let refused = stranger
        .post(&format!("/v1/jobs/{id}/close"), json!({}))
        .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);

    let closed = homeowner
        .post(&format!("/v1/jobs/{id}/close"), json!({}))
        .await;
    assert_eq!(closed.status, StatusCode::NO_CONTENT);

    // Closing twice is a conflict, not a silent success.
    let again = homeowner
        .post(&format!("/v1/jobs/{id}/close"), json!({}))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // And a closed job leaves the board.
    let mut anonymous = Client::new(router.clone());
    let board = anonymous.get("/v1/jobs").await;
    assert_eq!(board.json["jobs"].as_array().expect("array").len(), 0);
}

/// Every row exactly once, no repeats and nothing dropped. This is the property
/// the contractor directory's cursor gets wrong, so it is pinned here.
#[sqlx::test(migrations = "../../migrations")]
async fn pagination_returns_every_job_exactly_once(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;

    // Seeded through the repository: posting is capped at ten a day per
    // account, and this test is about the cursor, not the limiter.
    let poster = user_id(&pool, "owner@example.test").await;
    seed_jobs(&pool, poster, 25, "90026").await;

    let mut anonymous = Client::new(router.clone());
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;

    for _ in 0..10 {
        let path = match &cursor {
            Some(c) => format!("/v1/jobs?limit=7&cursor={c}"),
            None => "/v1/jobs?limit=7".to_owned(),
        };
        let page = anonymous.get(&path).await;
        assert_eq!(page.status, StatusCode::OK, "{:?}", page.json);

        for job in page.json["jobs"].as_array().expect("array") {
            seen.push(job["id"].as_str().expect("id").to_owned());
        }

        match page.json["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }

    assert_eq!(seen.len(), 25, "every job appears");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 25, "and none of them twice");
}

#[sqlx::test(migrations = "../../migrations")]
async fn filters_narrow_the_board_and_junk_is_reported_not_fatal(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    homeowner.post("/v1/jobs", a_job()).await;

    let mut plumbing = a_job();
    plumbing["title"] = json!("Repipe a bungalow");
    plumbing["trade"] = json!("plumber");
    plumbing["postal_code"] = json!("90401");
    homeowner.post("/v1/jobs", plumbing).await;

    let mut anonymous = Client::new(router.clone());

    let electrical = anonymous.get("/v1/jobs?trade=electrician").await;
    assert_eq!(electrical.json["jobs"].as_array().unwrap().len(), 1);

    let by_zip = anonymous.get("/v1/jobs?zip=90401").await;
    assert_eq!(by_zip.json["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(by_zip.json["jobs"][0]["title"], "Repipe a bungalow");

    // Santa Monica is far from Silver Lake, so a tight radius excludes it.
    let near_silver_lake = anonymous
        .get("/v1/jobs?lat=34.0781&lon=-118.2606&radius_m=3000")
        .await;
    assert_eq!(near_silver_lake.json["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(
        near_silver_lake.json["jobs"][0]["title"],
        "Rewire a 1920s duplex"
    );

    // A junk optional filter is dropped and named; the page still works.
    let forgiving = anonymous.get("/v1/jobs?zip=banana").await;
    assert_eq!(forgiving.status, StatusCode::OK);
    assert_eq!(forgiving.json["ignored_filters"][0], "zip");
    assert_eq!(forgiving.json["jobs"].as_array().unwrap().len(), 2);

    // A junk structural parameter is not forgiven.
    assert_eq!(
        anonymous.get("/v1/jobs?limit=lots").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        anonymous.get("/v1/jobs?cursor=not-a-cursor").await.status,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_post_is_validated_before_it_reaches_the_database(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;

    for (label, body) in [
        ("empty title", json!({ "title": "  ", "description": "x" })),
        ("empty description", json!({ "title": "x", "description": " " })),
        (
            "inverted budget",
            json!({ "title": "x", "description": "y",
                    "budget_min_cents": 900, "budget_max_cents": 100 }),
        ),
        (
            "bad zip",
            json!({ "title": "x", "description": "y", "postal_code": "9004" }),
        ),
        (
            "unknown trade",
            json!({ "title": "x", "description": "y", "trade": "astronaut" }),
        ),
        (
            "unknown timeline",
            json!({ "title": "x", "description": "y", "timeline": "eventually" }),
        ),
    ] {
        let response = homeowner.post("/v1/jobs", body).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{label} should be refused: {:?}",
            response.json
        );
    }
}

/// A job whose ZIP has no known centroid is still posted, and still listed —
/// it simply has no pin. Silently refusing the post, or dropping it from the
/// board, would both be worse than an unmapped row.
#[sqlx::test(migrations = "../../migrations")]
async fn a_job_in_an_unknown_zip_is_posted_but_unmapped(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;

    let mut job = a_job();
    job["postal_code"] = json!("99999");
    let posted = homeowner.post("/v1/jobs", job).await;

    assert_eq!(posted.status, StatusCode::CREATED, "{:?}", posted.json);
    assert_eq!(posted.json["location_precision"], "none");
    assert!(posted.json["lat"].is_null());

    let mut anonymous = Client::new(router.clone());
    let board = anonymous.get("/v1/jobs").await;
    assert_eq!(board.json["jobs"].as_array().unwrap().len(), 1);
}

/// Closing removes a job from the open web, not only from the list.
///
/// A detail page that kept resolving would leave the title and area online
/// after the poster asked for it to come down — and "cancelled" in particular
/// means take it down.
#[sqlx::test(migrations = "../../migrations")]
async fn a_closed_job_is_no_longer_publicly_readable(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    let posted = homeowner.post("/v1/jobs", a_job()).await;
    let id = posted.json["id"].as_str().expect("id").to_owned();

    let mut anonymous = Client::new(router.clone());
    assert_eq!(
        anonymous.get(&format!("/v1/jobs/{id}")).await.status,
        StatusCode::OK,
        "readable while open"
    );

    homeowner
        .post(&format!("/v1/jobs/{id}/close"), json!({ "status": "cancelled" }))
        .await;

    assert_eq!(
        anonymous.get(&format!("/v1/jobs/{id}")).await.status,
        StatusCode::NOT_FOUND,
        "and gone once cancelled"
    );

    // A contractor gets the same answer: the tier decides how much of a job is
    // shown, never whether a withdrawn one is shown at all.
    let mut contractor = Client::new(router.clone());
    contractor.register_contractor("sparks@example.test").await;
    assert_eq!(
        contractor.get(&format!("/v1/jobs/{id}")).await.status,
        StatusCode::NOT_FOUND
    );

    // The poster keeps their own record of it.
    let mine = homeowner.get("/v1/me/jobs").await;
    assert_eq!(mine.json.as_array().expect("array").len(), 1);
    assert_eq!(mine.json[0]["status"], "cancelled");
}
