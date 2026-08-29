//! Direct messaging: the gate, the ordering, and the safety controls.

mod common;

use common::{contractor_id, force_claim, router, seed_directory, user_id, Client};
use http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

/// A claimed contractor that has opted in to messages, and its owner's client.
async fn claimed_and_open(pool: &PgPool, router: &axum::Router) -> (uuid::Uuid, Client) {
    let id = contractor_id(pool, "1047382").await;

    let mut owner = Client::new(router.clone());
    owner.register_contractor("owner@example.test").await;
    force_claim(pool, id, user_id(pool, "owner@example.test").await).await;

    let opened = owner
        .send(
            http::Method::PATCH,
            &format!("/v1/contractors/{id}"),
            Some(json!({ "accepts_dm": true })),
        )
        .await;
    assert_eq!(opened.status, StatusCode::OK, "{:?}", opened.json);

    (id, owner)
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_homeowner_can_message_a_claimed_contractor_that_opted_in(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, mut owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router);
    homeowner.register("homeowner@example.test").await;

    let started = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.json);
    let conversation = started.json["id"].as_str().expect("id").to_owned();

    let sent = homeowner
        .post(
            &format!("/v1/conversations/{conversation}/messages"),
            json!({ "body": "Do you do garage conversions?" }),
        )
        .await;
    assert_eq!(sent.status, StatusCode::CREATED, "{:?}", sent.json);
    assert_eq!(sent.json["seq"], 1);

    // The contractor sees it, and can reply.
    let inbox = owner.get("/v1/conversations").await;
    assert_eq!(inbox.json.as_array().expect("array").len(), 1);
    assert_eq!(inbox.json[0]["unread"], 1);

    let polled = owner
        .get(&format!(
            "/v1/conversations/{conversation}/messages?after_seq=0"
        ))
        .await;
    assert_eq!(polled.json["messages"].as_array().expect("array").len(), 1);
    assert_eq!(
        polled.json["messages"][0]["body"],
        "Do you do garage conversions?"
    );
    assert_eq!(polled.json["next_seq"], 1);
    assert!(polled.json["poll_after_secs"].is_number());

    let replied = owner
        .post(
            &format!("/v1/conversations/{conversation}/messages"),
            json!({ "body": "Yes — ADUs are most of what we do." }),
        )
        .await;
    assert_eq!(
        replied.json["seq"], 2,
        "the sequence continues across senders"
    );

    // Reading catches the inbox up.
    owner
        .post(
            &format!("/v1/conversations/{conversation}/read"),
            json!({ "up_to_seq": 2 }),
        )
        .await;
    let inbox = owner.get("/v1/conversations").await;
    assert_eq!(inbox.json[0]["unread"], 0);
}

/// The gate: unclaimed listings and opted-out contractors are not messageable.
#[sqlx::test(migrations = "../../migrations")]
async fn messaging_is_refused_unless_the_listing_is_claimed_and_open(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("homeowner@example.test").await;

    // Unclaimed: nobody is behind it.
    let unclaimed = contractor_id(&pool, "983311").await;
    let refused = homeowner
        .post("/v1/conversations", json!({ "contractor_id": unclaimed }))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // Claimed but not opted in.
    let claimed = contractor_id(&pool, "1047382").await;
    let owner = cm_core::new_id();
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::insert(
        &mut conn,
        owner,
        "owner@example.test",
        "Owner",
        cm_db::repo::users::AccountType::Contractor,
    )
    .await
    .expect("user");
    drop(conn);
    force_claim(&pool, claimed, owner).await;

    let refused = homeowner
        .post("/v1/conversations", json!({ "contractor_id": claimed }))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // A listing that does not exist is a 404.
    let missing = homeowner
        .post(
            "/v1/conversations",
            json!({ "contractor_id": uuid::Uuid::now_v7() }),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

/// Two simultaneous "start a chat" requests must produce one conversation.
#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_starts_return_the_same_conversation(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, _owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router.clone());
    homeowner.register("homeowner@example.test").await;
    let session = homeowner.session_cookie().expect("session").to_owned();
    let csrf = homeowner.csrf_token().expect("csrf").to_owned();

    let start = || {
        let mut client = Client::new(router.clone());
        client.set_session(&session);
        client.set_csrf(&csrf);
        tokio::spawn(async move {
            client
                .post("/v1/conversations", json!({ "contractor_id": contractor }))
                .await
        })
    };

    let (first, second) = tokio::join!(start(), start());
    let first = first.expect("join");
    let second = second.expect("join");

    assert_eq!(first.status, StatusCode::CREATED);
    assert_eq!(second.status, StatusCode::CREATED);
    assert_eq!(
        first.json["id"], second.json["id"],
        "one conversation per pair, whatever the interleaving"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM conversations")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 1);
}

/// The polling contract: every message is seen exactly once, in order.
#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_sends_produce_a_gapless_sequence_a_poller_sees_once(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, _owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router.clone());
    homeowner.register("homeowner@example.test").await;
    let started = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    let conversation = started.json["id"].as_str().expect("id").to_owned();
    let session = homeowner.session_cookie().expect("session").to_owned();
    let csrf = homeowner.csrf_token().expect("csrf").to_owned();

    const SENDS: usize = 24;
    let mut tasks = Vec::new();
    for n in 0..SENDS {
        let mut client = Client::new(router.clone());
        client.set_session(&session);
        client.set_csrf(&csrf);
        let path = format!("/v1/conversations/{conversation}/messages");
        tasks.push(tokio::spawn(async move {
            client
                .post(&path, json!({ "body": format!("message {n}") }))
                .await
                .status
        }));
    }
    for task in tasks {
        assert_eq!(task.await.expect("join"), StatusCode::CREATED);
    }

    // Walk the conversation the way a polling client would.
    let mut seen: Vec<i64> = Vec::new();
    let mut cursor = 0i64;
    loop {
        let page = homeowner
            .get(&format!(
                "/v1/conversations/{conversation}/messages?after_seq={cursor}&limit=5"
            ))
            .await;
        let messages = page.json["messages"].as_array().expect("array");
        if messages.is_empty() {
            break;
        }
        for message in messages {
            seen.push(message["seq"].as_i64().expect("seq"));
        }
        cursor = page.json["next_seq"].as_i64().expect("next_seq");
    }

    assert_eq!(
        seen,
        (1..=SENDS as i64).collect::<Vec<i64>>(),
        "the sequence must be gapless, ordered, and free of duplicates"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_non_participant_cannot_see_or_join_a_conversation(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, _owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router.clone());
    homeowner.register("homeowner@example.test").await;
    let started = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    let conversation = started.json["id"].as_str().expect("id").to_owned();

    let mut outsider = Client::new(router);
    outsider.register("outsider@example.test").await;

    // 404, not 403: an outsider must not learn the conversation exists.
    assert_eq!(
        outsider
            .get(&format!("/v1/conversations/{conversation}/messages"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        outsider
            .post(
                &format!("/v1/conversations/{conversation}/messages"),
                json!({ "body": "let me in" })
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        outsider
            .get("/v1/conversations")
            .await
            .json
            .as_array()
            .expect("array")
            .len(),
        0
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_block_stops_messages_in_both_directions_but_keeps_the_history(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, mut owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router.clone());
    homeowner.register("homeowner@example.test").await;
    let started = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    let conversation = started.json["id"].as_str().expect("id").to_owned();
    homeowner
        .post(
            &format!("/v1/conversations/{conversation}/messages"),
            json!({ "body": "first message" }),
        )
        .await;

    // The contractor blocks the homeowner.
    let homeowner_id = user_id(&pool, "homeowner@example.test").await;
    let blocked = owner
        .send(
            http::Method::PUT,
            &format!("/v1/blocks/{homeowner_id}"),
            Some(json!({ "reason": "spam" })),
        )
        .await;
    assert_eq!(blocked.status, StatusCode::NO_CONTENT);

    // Neither side can send now.
    assert_eq!(
        homeowner
            .post(
                &format!("/v1/conversations/{conversation}/messages"),
                json!({ "body": "hello?" })
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        owner
            .post(
                &format!("/v1/conversations/{conversation}/messages"),
                json!({ "body": "go away" })
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // But the history survives — a report depends on it.
    let history = owner
        .get(&format!("/v1/conversations/{conversation}/messages"))
        .await;
    assert_eq!(history.json["messages"].as_array().expect("array").len(), 1);

    // A new conversation cannot be started either, and the refusal is the same
    // one an opted-out contractor gives: being told you are blocked is an
    // invitation to work around it.
    let mut second = Client::new(router.clone());
    second.set_session(homeowner.session_cookie().expect("session"));
    second.set_csrf(homeowner.csrf_token().expect("csrf"));
    assert_eq!(
        second
            .post("/v1/conversations", json!({ "contractor_id": contractor }))
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Unblocking restores it.
    assert_eq!(
        owner
            .send(
                http::Method::DELETE,
                &format!("/v1/blocks/{homeowner_id}"),
                None
            )
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        homeowner
            .post(
                &format!("/v1/conversations/{conversation}/messages"),
                json!({ "body": "thanks" })
            )
            .await
            .status,
        StatusCode::CREATED
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_report_reaches_moderation_without_notifying_the_reported(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, mut owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router.clone());
    homeowner.register("homeowner@example.test").await;
    let started = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    let conversation = started.json["id"].as_str().expect("id").to_owned();
    let sent = owner
        .post(
            &format!("/v1/conversations/{conversation}/messages"),
            json!({ "body": "pay me off-platform" }),
        )
        .await;
    let message_id = sent.json["id"].as_str().expect("id").to_owned();

    let reported = homeowner
        .post(
            "/v1/reports",
            json!({
                "conversation_id": conversation,
                "message_id": message_id,
                "reason": "off_platform_payment",
                "detail": "asked for a bank transfer"
            }),
        )
        .await;
    assert_eq!(reported.status, StatusCode::CREATED, "{:?}", reported.json);
    assert_eq!(reported.json["status"], "open");

    // Reporting the same message twice is a conflict.
    let again = homeowner
        .post(
            "/v1/reports",
            json!({
                "conversation_id": conversation,
                "message_id": message_id,
                "reason": "spam"
            }),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // An unknown reason is refused with the list of valid ones.
    let bad = homeowner
        .post(
            "/v1/reports",
            json!({ "conversation_id": conversation, "reason": "vibes" }),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.json["error"]["message"]
        .as_str()
        .expect("message")
        .contains("harassment"));

    // Moderators see it; ordinary accounts do not.
    assert_eq!(
        owner.get("/v1/admin/reports").await.status,
        StatusCode::FORBIDDEN
    );

    let mut admin = Client::new(router);
    admin.register("admin@example.test").await;
    let admin_id = user_id(&pool, "admin@example.test").await;
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::grant_role(
        &mut conn,
        admin_id,
        cm_db::repo::users::Role::Moderator,
        None,
    )
    .await
    .expect("grant");
    drop(conn);

    let queue = admin.get("/v1/admin/reports").await;
    assert_eq!(queue.status, StatusCode::OK);
    assert_eq!(queue.json.as_array().expect("array").len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn conversation_creation_is_rate_limited_per_account(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    // Ten claimed, opted-in contractors would be needed to exhaust the limit
    // honestly; instead the limit is exercised against one, since the bucket is
    // per account rather than per target.
    let (contractor, _owner) = claimed_and_open(&pool, &router).await;
    let mut homeowner = Client::new(router.clone());
    homeowner.register("homeowner@example.test").await;

    // The first call creates it; the rest find the same one, and every call
    // still counts.
    for _ in 0..10 {
        let response = homeowner
            .post("/v1/conversations", json!({ "contractor_id": contractor }))
            .await;
        assert_eq!(response.status, StatusCode::CREATED);
    }

    let limited = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;

    // Windows are aligned to the epoch, so a burst that straddles a boundary
    // legitimately gets a fresh allowance. Asserting unconditionally would make
    // this test fail once every few hundred runs for a correct reason.
    let windows: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT window_start) FROM rate_limit_counters WHERE count > 1",
    )
    .fetch_one(&pool)
    .await
    .expect("count windows");

    if windows <= 1 {
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers.get(http::header::RETRY_AFTER).is_some());
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_empty_or_oversized_message_is_refused(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, _owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router);
    homeowner.register("homeowner@example.test").await;
    let started = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    let conversation = started.json["id"].as_str().expect("id").to_owned();
    let path = format!("/v1/conversations/{conversation}/messages");

    for body in ["", "   ", &"x".repeat(4001)] {
        let response = homeowner.post(&path, json!({ "body": body })).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "a {}-character body should be refused",
            body.len()
        );
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn messaging_endpoints_require_a_session_and_a_csrf_token(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, _owner) = claimed_and_open(&pool, &router).await;

    let anonymous = Client::new(router.clone())
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let mut signed_in = Client::new(router.clone());
    signed_in.register("homeowner@example.test").await;
    let session = signed_in.session_cookie().expect("session").to_owned();

    let mut without_csrf = Client::new(router).without_csrf();
    without_csrf.set_session(&session);
    assert_eq!(
        without_csrf
            .post("/v1/conversations", json!({ "contractor_id": contractor }))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

/// A retracted message keeps its place, and only its sender may retract it.
///
/// The sequence is the poll cursor. If a delete removed the row, a client
/// resuming from `after_seq` could not tell "seq 2 was deleted" from "seq 2 has
/// not arrived yet", and would either stall or skip. So the row survives as a
/// tombstone with its body replaced, and every seq around it is unchanged.
#[sqlx::test(migrations = "../../migrations")]
async fn a_deleted_message_keeps_its_place_in_the_sequence(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, mut owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router);
    homeowner.register("homeowner@example.test").await;

    let started = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    let conversation = started.json["id"].as_str().expect("id").to_owned();

    let messages = format!("/v1/conversations/{conversation}/messages");
    for body in ["first", "second, to be retracted", "third"] {
        let sent = homeowner.post(&messages, json!({ "body": body })).await;
        assert_eq!(sent.status, StatusCode::CREATED, "{:?}", sent.json);
    }

    let page = homeowner.get(&messages).await;
    let second = page.json["messages"][1]["id"]
        .as_str()
        .expect("the second message")
        .to_owned();

    // The recipient may not delete what they did not write. Answered 404 rather
    // than 403 so a probe learns nothing about what exists.
    let refused = owner
        .delete(&format!(
            "/v1/conversations/{conversation}/messages/{second}"
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "a recipient deleted the sender's message: {:?}",
        refused.json
    );

    let removed = homeowner
        .delete(&format!(
            "/v1/conversations/{conversation}/messages/{second}"
        ))
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{:?}", removed.json);

    // Three messages still, in the same order, with the middle one tombstoned.
    let after = owner.get(&messages).await;
    let rows = after.json["messages"].as_array().expect("messages");
    assert_eq!(rows.len(), 3, "a delete left a hole in the sequence");

    assert_eq!(rows[0]["seq"], 1);
    assert_eq!(rows[0]["body"], "first");
    assert_eq!(rows[0]["deleted"], false);

    assert_eq!(rows[1]["seq"], 2, "the tombstone lost its place");
    assert_eq!(rows[1]["body"], "[removed]");
    assert_eq!(rows[1]["deleted"], true);

    assert_eq!(rows[2]["seq"], 3);
    assert_eq!(rows[2]["body"], "third");

    // Deleting twice is not an error worth inventing state for, but it must not
    // report success either — the row no longer matches.
    let again = homeowner
        .delete(&format!(
            "/v1/conversations/{conversation}/messages/{second}"
        ))
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

/// A contractor can answer posted work, and the reply lands in the same thread.
///
/// The pair identifies a conversation, not the direction it opened from, so a
/// homeowner who later writes to the same contractor must land in the thread
/// that already exists rather than starting a second one beside it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_contractor_can_answer_a_posted_job(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let (contractor, mut owner) = claimed_and_open(&pool, &router).await;

    let mut homeowner = Client::new(router.clone());
    homeowner.register("poster@example.test").await;

    let posted = homeowner
        .post(
            "/v1/jobs",
            json!({
                "title": "Rewire a 1920s duplex",
                "description": "Knob and tube throughout. Needs a full rewire and a new panel.",
                "trade": "electrician",
                "build_type": "replacement",
                "job_size": "Whole house, roughly 1,800 sq ft",
                "postal_code": "90026",
                "budget": { "min_cents": 800_000, "max_cents": 1_500_000 },
                "timeline": "within_2_weeks"
            }),
        )
        .await;
    assert_eq!(posted.status, StatusCode::CREATED, "{:?}", posted.json);
    let job = posted.json["id"].as_str().expect("job id").to_owned();

    // Exactly one selector, or the request is meaningless.
    let both = owner
        .post(
            "/v1/conversations",
            json!({ "contractor_id": contractor, "job_id": job }),
        )
        .await;
    assert_eq!(both.status, StatusCode::BAD_REQUEST, "{:?}", both.json);

    // A homeowner cannot answer a job — that is the contractor's direction.
    let wrong_side = homeowner
        .post("/v1/conversations", json!({ "job_id": job }))
        .await;
    assert_eq!(wrong_side.status, StatusCode::FORBIDDEN);

    let started = owner
        .post("/v1/conversations", json!({ "job_id": job }))
        .await;
    assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.json);
    let conversation = started.json["id"].as_str().expect("id").to_owned();
    assert_eq!(
        started.json["contractor_id"],
        contractor.to_string(),
        "the thread is tagged with the writing contractor's listing, so the \
         homeowner can see which business is contacting them"
    );

    let sent = owner
        .post(
            &format!("/v1/conversations/{conversation}/messages"),
            json!({ "body": "Saw your rewire post — I do knob and tube. Free next week." }),
        )
        .await;
    assert_eq!(sent.status, StatusCode::CREATED, "{:?}", sent.json);

    // The homeowner has it, and replying needs nothing new.
    let inbox = homeowner.get("/v1/conversations").await;
    assert_eq!(inbox.json[0]["id"], conversation);
    assert_eq!(inbox.json[0]["unread"], 1);

    let replied = homeowner
        .post(
            &format!("/v1/conversations/{conversation}/messages"),
            json!({ "body": "Yes please — what does the panel usually run?" }),
        )
        .await;
    assert_eq!(replied.status, StatusCode::CREATED, "{:?}", replied.json);
    assert_eq!(replied.json["seq"], 2);

    // The other direction finds the same thread rather than forking one.
    let from_the_other_side = homeowner
        .post("/v1/conversations", json!({ "contractor_id": contractor }))
        .await;
    assert_eq!(
        from_the_other_side.json["id"], conversation,
        "opening from the other direction forked a second conversation"
    );
}

/// A contractor with no approved listing cannot message posters.
///
/// Without this, any account that signed up could write to every homeowner who
/// posted work, and the homeowner would have no licence to check them against.
#[sqlx::test(migrations = "../../migrations")]
async fn answering_a_job_needs_an_approved_listing(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());

    let mut homeowner = Client::new(router.clone());
    homeowner.register("poster2@example.test").await;
    let posted = homeowner
        .post(
            "/v1/jobs",
            json!({
                "title": "Replace a water heater",
                "description": "The old one is leaking from the base and needs replacing soon.",
                "trade": "other",
                "build_type": "replacement",
                "job_size": "One unit",
                "postal_code": "90026",
                "budget": "unsure",
                "timeline": "asap"
            }),
        )
        .await;
    let job = posted.json["id"].as_str().expect("job id").to_owned();

    let mut unclaimed = Client::new(router);
    unclaimed
        .register_contractor("nolisting@example.test")
        .await;

    let refused = unclaimed
        .post("/v1/conversations", json!({ "job_id": job }))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a contractor with no approved listing reached a homeowner: {:?}",
        refused.json
    );
}
