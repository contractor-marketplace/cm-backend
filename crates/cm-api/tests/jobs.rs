//! Posting a job, and browsing the board.
//!
//! There is one browse projection and every caller gets it, so the assertions
//! here are about what the projection itself carries — a first name and never a
//! surname or an email — and about what an account may do: only a homeowner
//! posts, only the poster closes.

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
        "build_type": "replacement",
        "job_size": "Whole house, roughly 1,800 sq ft",
        "postal_code": "90026",
        "budget": { "min_cents": 800_000, "max_cents": 1_500_000 },
        "timeline": "within_2_weeks"
    })
}

/// The same post with every escape hatch taken. Everything is still answered —
/// that is the point of the escapes.
fn a_job_with_every_unsure() -> serde_json::Value {
    json!({
        "title": "Something is leaking under the house",
        "description": "Water is pooling in the crawlspace and I cannot tell where \
                        it is coming from. Might be the main.",
        "trade": "other",
        "build_type": "unsure",
        "job_size": "No idea, sorry",
        "postal_code": "90026",
        "budget": "unsure",
        "timeline": "unsure"
    })
}

/// A 1x1 PNG. Smallest thing that survives the normaliser.
fn a_tiny_png() -> Vec<u8> {
    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(PIXEL)
        .expect("a valid base64 pixel")
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
    assert_eq!(posted.json["build_type"], "replacement");
    assert_eq!(posted.json["job_size"], "Whole house, roughly 1,800 sq ft");
    assert_eq!(posted.json["timeline"], "within_2_weeks");
    assert_eq!(posted.json["location_precision"], "zip_centroid");
    assert!(posted.json["lat"].is_number(), "a known ZIP places the job");

    let mut contractor = Client::new(router.clone());
    contractor.register_contractor("sparks@example.test").await;
    let board = contractor.get("/v1/jobs").await;
    assert_eq!(board.status, StatusCode::OK, "{:?}", board.json);

    let first = &board.json["jobs"][0];
    assert_eq!(first["title"], "Rewire a 1920s duplex");
    assert!(
        first["description"]
            .as_str()
            .unwrap()
            .contains("Knob and tube"),
        "the description is part of the listing"
    );
    assert_eq!(
        first["poster_first_name"], "Test",
        "a first name, from the display name 'Test Person'"
    );
}

/// The board is one listing, and a signed-out visitor gets all of it.
///
/// What the listing withholds it withholds from everyone: the poster is a first
/// name, never a surname or an email, and a job has no address to leak because
/// the table has no column for one.
#[sqlx::test(migrations = "../../migrations")]
async fn an_anonymous_caller_sees_the_same_job_as_everyone_else(pool: PgPool) {
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
        let rendered = response.json.to_string();

        for present in [
            "Rewire a 1920s duplex", // the title
            "Knob and tube",         // the description
            "\"poster_first_name\":\"Test\"",
        ] {
            assert!(rendered.contains(present), "{path} is missing {present}");
        }

        for leaked in [
            "owner@example",     // the address
            "Test Person",       // the full display name
            "posted_by_user_id", // the poster's identity
        ] {
            assert!(
                !rendered.contains(leaked),
                "{path} leaked {leaked}: {rendered}"
            );
        }
    }
}

/// Anonymous, homeowner and contractor get byte-identical jobs.
///
/// Pinned rather than assumed: the board used to branch on the caller, and this
/// is the property that replaced it.
#[sqlx::test(migrations = "../../migrations")]
async fn every_account_type_sees_the_same_board(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut poster = Client::new(router.clone());
    poster.register("owner@example.test").await;
    poster.post("/v1/jobs", a_job()).await;

    let mut homeowner = Client::new(router.clone());
    homeowner.register("nosy@example.test").await;
    let mut contractor = Client::new(router.clone());
    contractor.register_contractor("sparks@example.test").await;
    let mut anonymous = Client::new(router.clone());

    let anonymous_jobs = anonymous.get("/v1/jobs").await.json["jobs"].clone();
    assert_eq!(anonymous_jobs.as_array().expect("array").len(), 1);

    for (label, board) in [
        ("a signed-in homeowner", homeowner.get("/v1/jobs").await),
        ("a contractor", contractor.get("/v1/jobs").await),
        ("the poster themselves", poster.get("/v1/jobs").await),
    ] {
        assert_eq!(
            board.json["jobs"], anonymous_jobs,
            "{label} sees a different board than a stranger"
        );
    }
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
    assert!(mine.json[0]["description"]
        .as_str()
        .unwrap()
        .contains("Knob"));
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
        (
            "empty description",
            json!({ "title": "x", "description": " " }),
        ),
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
        .post(
            &format!("/v1/jobs/{id}/close"),
            json!({ "status": "cancelled" }),
        )
        .await;

    assert_eq!(
        anonymous.get(&format!("/v1/jobs/{id}")).await.status,
        StatusCode::NOT_FOUND,
        "and gone once cancelled"
    );

    // A session is not a way back in: a withdrawn job is withdrawn from
    // everyone, and being signed in does not resurrect it.
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

/// Every field is required, and the message says which one.
///
/// Dropping a field entirely must fail at the request shape, before any of the
/// domain rules run — that is what makes it safe for the layers below to read a
/// `None` as "the poster chose the escape hatch" rather than "the client forgot
/// to send it".
#[sqlx::test(migrations = "../../migrations")]
async fn a_missing_field_is_refused(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;

    for field in [
        "title",
        "description",
        "trade",
        "build_type",
        "job_size",
        "postal_code",
        "timeline",
        "budget",
    ] {
        let mut body = a_job();
        body.as_object_mut().expect("object").remove(field);

        let refused = homeowner.post("/v1/jobs", body).await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "a job with no {field} was accepted: {:?}",
            refused.json
        );
    }
}

/// "I don't know" is an answer, not a blank. Each escape posts, and each one
/// round-trips as the thing the poster actually chose.
#[sqlx::test(migrations = "../../migrations")]
async fn unsure_is_a_valid_answer_everywhere_it_is_offered(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;

    let posted = homeowner.post("/v1/jobs", a_job_with_every_unsure()).await;
    assert_eq!(posted.status, StatusCode::CREATED, "{:?}", posted.json);

    // "other" is recorded as no trade, which is what the schema stores.
    assert!(posted.json["trade"].is_null(), "\"other\" means no trade");
    assert_eq!(posted.json["build_type"], "unsure");
    assert_eq!(posted.json["timeline"], "unsure");
    assert_eq!(posted.json["job_size"], "No idea, sorry");
    assert!(posted.json["budget_min_cents"].is_null());
    assert!(posted.json["budget_max_cents"].is_null());

    // And it is a real listing, not a second-class one.
    let mut anonymous = Client::new(router.clone());
    let board = anonymous.get("/v1/jobs").await;
    assert_eq!(board.json["jobs"].as_array().expect("array").len(), 1);
}

/// A half-filled range is neither a range nor an admission, and the schema no
/// longer represents one — so the API must not accept one either.
#[sqlx::test(migrations = "../../migrations")]
async fn a_budget_is_a_whole_range_or_nothing(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;

    for (label, budget) in [
        ("only a minimum", json!({ "min_cents": 100_000 })),
        ("only a maximum", json!({ "max_cents": 100_000 })),
        ("inverted", json!({ "min_cents": 900, "max_cents": 100 })),
        ("negative", json!({ "min_cents": -1, "max_cents": 100 })),
        ("a word that is not unsure", json!("dunno")),
        ("empty", json!({})),
    ] {
        let mut body = a_job();
        body["budget"] = budget;
        let refused = homeowner.post("/v1/jobs", body).await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{label} was accepted: {:?}",
            refused.json
        );
    }
}

/// One sentence, floor-wise. "New panel" as an entire brief wastes the time of
/// every contractor who opens it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_thin_description_is_refused_with_a_count(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;

    let mut body = a_job();
    body["description"] = json!("x".repeat(49));
    let refused = homeowner.post("/v1/jobs", body).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(
        refused.json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("49"),
        "the message should say how many they wrote: {:?}",
        refused.json
    );

    // And exactly 50 is enough.
    let mut body = a_job();
    body["description"] = json!("x".repeat(50));
    assert_eq!(
        homeowner.post("/v1/jobs", body).await.status,
        StatusCode::CREATED
    );
}

/// A ZIP is required, but an *unknown* ZIP still posts. Requiring the field and
/// requiring our ZCTA import to know it are different things.
#[sqlx::test(migrations = "../../migrations")]
async fn a_zip_is_required_but_need_not_be_one_we_know(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;

    for bad in ["", "9004", "9004x", "900265"] {
        let mut body = a_job();
        body["postal_code"] = json!(bad);
        assert_eq!(
            homeowner.post("/v1/jobs", body).await.status,
            StatusCode::BAD_REQUEST,
            "ZIP {bad:?} should be refused"
        );
    }

    let mut body = a_job();
    body["postal_code"] = json!("99999");
    let posted = homeowner.post("/v1/jobs", body).await;
    assert_eq!(posted.status, StatusCode::CREATED, "{:?}", posted.json);
    assert_eq!(posted.json["location_precision"], "none");
    assert!(posted.json["lat"].is_null(), "unmapped, but posted");
}

/// Photos: uploaded by the poster, capped, ordered, and visible to everyone.
#[sqlx::test(migrations = "../../migrations")]
async fn photos_are_attached_by_the_poster_and_shown_to_everyone(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    let posted = homeowner.post("/v1/jobs", a_job()).await;
    let id = posted.json["id"].as_str().expect("id").to_owned();
    assert_eq!(
        posted.json["photos"].as_array().expect("array").len(),
        0,
        "a job starts with no photos, and that is a normal state"
    );

    let first = homeowner
        .post_file(&format!("/v1/jobs/{id}/photos"), a_tiny_png())
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.json);
    assert!(first.json["url"].is_string());
    assert_eq!(first.json["width"], 1);

    homeowner
        .post_file(&format!("/v1/jobs/{id}/photos"), a_tiny_png())
        .await;

    // Anyone can see them: a job's photos are exactly as public as its
    // description.
    let mut anonymous = Client::new(router.clone());
    let seen = anonymous.get(&format!("/v1/jobs/{id}")).await;
    let photos = seen.json["photos"].as_array().expect("array");
    assert_eq!(photos.len(), 2);
    assert_ne!(photos[0]["id"], photos[1]["id"], "two distinct photos");

    // A stranger cannot add one, and gets a 404 rather than a 403 — the same
    // rule closing uses.
    let mut stranger = Client::new(router.clone());
    stranger.register("stranger@example.test").await;
    assert_eq!(
        stranger
            .post_file(&format!("/v1/jobs/{id}/photos"), a_tiny_png())
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // The poster can take one down; a stranger cannot.
    let photo_id = photos[0]["id"].as_str().expect("id").to_owned();
    assert_eq!(
        stranger
            .delete(&format!("/v1/jobs/{id}/photos/{photo_id}"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        homeowner
            .delete(&format!("/v1/jobs/{id}/photos/{photo_id}"))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        anonymous.get(&format!("/v1/jobs/{id}")).await.json["photos"]
            .as_array()
            .expect("array")
            .len(),
        1
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_photo_upload_is_capped_and_must_be_an_image(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    let posted = homeowner.post("/v1/jobs", a_job()).await;
    let id = posted.json["id"].as_str().expect("id").to_owned();

    // Not an image, whatever it claims to be.
    let refused = homeowner
        .post_file(
            &format!("/v1/jobs/{id}/photos"),
            b"PK\x03\x04 not a photo".to_vec(),
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.json
    );

    for _ in 0..8 {
        homeowner
            .post_file(&format!("/v1/jobs/{id}/photos"), a_tiny_png())
            .await;
    }

    let ninth = homeowner
        .post_file(&format!("/v1/jobs/{id}/photos"), a_tiny_png())
        .await;
    assert_eq!(ninth.status, StatusCode::BAD_REQUEST);
    assert!(
        ninth.json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("8"),
        "the message should name the cap: {:?}",
        ninth.json
    );
}

/// Cancelling means take it down, and that has to include the photos — a job
/// page that 404s while its images stay fetchable is not withdrawn.
#[sqlx::test(migrations = "../../migrations")]
async fn cancelling_a_job_removes_its_photos(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("owner@example.test").await;
    let posted = homeowner.post("/v1/jobs", a_job()).await;
    let id = posted.json["id"].as_str().expect("id").to_owned();
    homeowner
        .post_file(&format!("/v1/jobs/{id}/photos"), a_tiny_png())
        .await;

    homeowner
        .post(
            &format!("/v1/jobs/{id}/close"),
            json!({ "status": "cancelled" }),
        )
        .await;

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM job_photos WHERE job_id = $1")
        .bind(uuid::Uuid::parse_str(&id).expect("uuid"))
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(remaining, 0, "a cancelled job keeps no photo rows");

    // Closing is different: the work happened, and the poster keeps the record.
    let second = homeowner.post("/v1/jobs", a_job()).await;
    let second_id = second.json["id"].as_str().expect("id").to_owned();
    homeowner
        .post_file(&format!("/v1/jobs/{second_id}/photos"), a_tiny_png())
        .await;
    homeowner
        .post(&format!("/v1/jobs/{second_id}/close"), json!({}))
        .await;

    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM job_photos WHERE job_id = $1")
        .bind(uuid::Uuid::parse_str(&second_id).expect("uuid"))
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(kept, 1, "a completed job keeps its record");
}

/// A closed job can be put back on the board; a cancelled one cannot.
///
/// The asymmetry is the point. Closing takes nothing away, so it is safe to
/// undo — a poster who closed in haste, or whose contractor fell through, gets
/// their listing back. Cancelling deletes the photos from the object store, and
/// nothing can undelete them, so reopening would republish a job quietly
/// missing the pictures a contractor was meant to see.
#[sqlx::test(migrations = "../../migrations")]
async fn a_closed_job_can_be_reopened_but_a_cancelled_one_cannot(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("reopen@example.test").await;

    let job = homeowner.post("/v1/jobs", a_job()).await.json["id"]
        .as_str()
        .expect("id")
        .to_owned();

    // Closed, then off the public board.
    let closed = homeowner
        .post(
            &format!("/v1/jobs/{job}/close"),
            json!({ "status": "closed" }),
        )
        .await;
    assert_eq!(closed.status, StatusCode::NO_CONTENT, "{:?}", closed.json);
    assert_eq!(
        Client::new(router.clone())
            .get(&format!("/v1/jobs/{job}"))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a closed job stays on the public board"
    );

    let reopened = homeowner
        .post(&format!("/v1/jobs/{job}/reopen"), json!({}))
        .await;
    assert_eq!(
        reopened.status,
        StatusCode::NO_CONTENT,
        "{:?}",
        reopened.json
    );

    let public = Client::new(router.clone())
        .get(&format!("/v1/jobs/{job}"))
        .await;
    assert_eq!(
        public.status,
        StatusCode::OK,
        "reopening did not republish it"
    );
    assert_eq!(public.json["status"], "open");

    // Reopening an already-open job changes nothing and says so.
    let again = homeowner
        .post(&format!("/v1/jobs/{job}/reopen"), json!({}))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // Cancelled is a one-way door.
    let cancelled = homeowner
        .post(
            &format!("/v1/jobs/{job}/close"),
            json!({ "status": "cancelled" }),
        )
        .await;
    assert_eq!(cancelled.status, StatusCode::NO_CONTENT);

    let refused = homeowner
        .post(&format!("/v1/jobs/{job}/reopen"), json!({}))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "a cancelled job was reopened, and its photos are already gone: {:?}",
        refused.json
    );

    // Somebody else's job is not found, not forbidden.
    let mut other = Client::new(router);
    other.register("nosy@example.test").await;
    assert_eq!(
        other
            .post(&format!("/v1/jobs/{job}/reopen"), json!({}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
