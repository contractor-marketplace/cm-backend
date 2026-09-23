//! Claiming a listing, and the verified badge that follows from it.

mod common;

use common::{contractor_id, router, seed_directory, user_id, Client};
use http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

async fn make_admin(pool: &PgPool, email: &str) {
    let mut conn = pool.acquire().await.expect("connection");
    let id = user_id(pool, email).await;
    cm_db::repo::users::grant_role(&mut conn, id, cm_db::repo::users::Role::Admin, None)
        .await
        .expect("grant");
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_approved_claim_grants_ownership_and_the_badge(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let router = router(pool.clone());

    let mut claimant = Client::new(router.clone());
    claimant.register_contractor("marisol@example.test").await;

    let opened = claimant
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review", "evidence": { "note": "I own this business" } }),
        )
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{:?}", opened.json);

    // Early-launch: the claim is approved in the same request, with the
    // decision attributed to the claimant, never to a phantom moderator.
    assert_eq!(opened.json["status"], "approved");
    assert!(
        !opened.json["decided_at"].is_null(),
        "an auto-approved claim must carry the time it was decided"
    );

    // Ownership and the badge follow immediately — the badge only because
    // this fixture's licence is active. See the expired-licence test for the
    // half auto-approval does NOT grant.
    let mut anyone = Client::new(router.clone());
    let after = anyone.get(&format!("/v1/contractors/{id}")).await;
    assert_eq!(after.json["verified"], true);
    assert_eq!(after.json["is_claimed"], true);
    // Claiming opens the listing to messages; the owner can close it later.
    assert_eq!(after.json["accepts_dm"], true);

    // The claimant now holds the contractor role.
    let me = claimant.get("/v1/me").await;
    assert_eq!(me.json["roles"], json!(["contractor"]));

    // Nothing waits for a moderator.
    let mut admin = Client::new(router.clone());
    admin.register("admin@example.test").await;
    make_admin(&pool, "admin@example.test").await;
    // The role only takes effect on the next request, which re-reads it.
    let queue = admin.get("/v1/admin/claims").await;
    assert_eq!(queue.status, StatusCode::OK, "{:?}", queue.json);
    assert_eq!(queue.json.as_array().expect("array").len(), 0);
}

/// A licence that is not active never produces a badge, however good the claim.
#[sqlx::test(migrations = "../../migrations")]
async fn an_expired_licence_is_never_verified(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "445190").await; // expired in the fixture
    let router = router(pool.clone());

    let mut claimant = Client::new(router.clone());
    claimant.register_contractor("roofer@example.test").await;
    let opened = claimant
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review" }),
        )
        .await;

    // Auto-approval grants ownership, never the badge: the licence is expired.
    assert_eq!(opened.status, StatusCode::CREATED, "{:?}", opened.json);
    assert_eq!(opened.json["status"], "approved");

    let after = Client::new(router.clone())
        .get(&format!("/v1/contractors/{id}"))
        .await;
    assert_eq!(after.json["verified"], false);
    assert_eq!(after.json["is_claimed"], true);

    let reason: String =
        sqlx::query_scalar("SELECT verification_reason FROM contractors WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("reason");
    assert!(reason.contains("expired"), "{reason}");
}

/// An import that changes a licence must move the badge with it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_licence_going_inactive_removes_the_badge(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;

    let owner = cm_core::new_id();
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::insert(
        &mut conn,
        owner,
        Some("owner@example.test"),
        "Owner",
        cm_db::repo::users::AccountType::Contractor,
    )
    .await
    .expect("user");
    drop(conn);
    common::force_claim(&pool, id, owner).await;

    let mut client = Client::new(router(pool.clone()));
    assert_eq!(
        client.get(&format!("/v1/contractors/{id}")).await.json["verified"],
        true
    );

    // The register says the licence lapsed.
    sqlx::query("UPDATE license_records SET status = 'inactive' WHERE license_no = '1047382'")
        .execute(&pool)
        .await
        .expect("lapse");
    cm_domain::verification::recompute_all(&pool, 100)
        .await
        .expect("recompute");

    let after = client.get(&format!("/v1/contractors/{id}")).await;
    assert_eq!(after.json["verified"], false);
    assert!(
        after.json["is_claimed"].as_bool().expect("claimed"),
        "still owned"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn two_simultaneous_claims_produce_exactly_one_owner(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let router = router(pool.clone());

    // Two people claim the same listing at the same instant. With
    // auto-approval the race moved from the moderation queue to the open
    // itself: both pass the "is it claimed" pre-check, and the partial unique
    // index `contractor_claims_one_approved_per_contractor` decides who owns.
    let mut first = Client::new(router.clone());
    first.register_contractor("first@example.test").await;
    let mut second = Client::new(router.clone());
    second.register_contractor("second@example.test").await;

    let claim = |client: &Client| {
        let mut racer = Client::new(router.clone());
        let session = client.session_cookie().expect("session").to_owned();
        let csrf = client.csrf_token().expect("csrf").to_owned();
        racer.set_session(&session);
        racer.set_csrf(&csrf);
        let path = format!("/v1/contractors/{id}/claims");
        tokio::spawn(async move {
            racer
                .post(&path, json!({ "method": "manual_review" }))
                .await
                .status
        })
    };

    let (first_status, second_status) = tokio::join!(claim(&first), claim(&second));
    let statuses = [first_status.expect("join"), second_status.expect("join")];

    let successes = statuses.iter().filter(|s| s.is_success()).count();
    assert_eq!(successes, 1, "exactly one claim may win: {statuses:?}");
    assert!(
        statuses.contains(&StatusCode::CONFLICT),
        "the loser is told, not silently ignored: {statuses:?}"
    );

    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM contractor_claims WHERE status = 'approved' AND contractor_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(owners, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_claim_needs_a_session_and_moderation_needs_a_role(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let router = router(pool.clone());

    let anonymous = Client::new(router.clone())
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review" }),
        )
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    // A homeowner account cannot claim a listing at all: the two sides of the
    // marketplace are mutually exclusive, and this is the contractor's side.
    let mut homeowner = Client::new(router.clone());
    homeowner.register("homeowner@example.test").await;
    assert_eq!(
        homeowner
            .post(
                &format!("/v1/contractors/{id}/claims"),
                json!({ "method": "manual_review" }),
            )
            .await
            .status,
        StatusCode::FORBIDDEN,
        "a homeowner account cannot claim a listing"
    );

    // A contractor account may claim, but claiming confers no moderation
    // power — that is what the rest of this test pins down.
    let mut ordinary = Client::new(router.clone());
    ordinary.register_contractor("ordinary@example.test").await;
    assert_eq!(
        ordinary.get("/v1/admin/claims").await.status,
        StatusCode::FORBIDDEN,
        "an ordinary account cannot see the moderation queue"
    );

    let opened = ordinary
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review" }),
        )
        .await;
    let claim_id = opened.json["id"].as_str().expect("id").to_owned();

    assert_eq!(
        ordinary
            .post(
                &format!("/v1/admin/claims/{claim_id}/decide"),
                json!({ "approve": true })
            )
            .await
            .status,
        StatusCode::FORBIDDEN,
        "and cannot approve their own claim"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn withdrawal_applies_only_to_pending_claims(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let router = router(pool.clone());

    let mut claimant = Client::new(router.clone());
    claimant.register_contractor("claimant@example.test").await;
    let opened = claimant
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review" }),
        )
        .await;
    let claim_id = opened.json["id"].as_str().expect("id").to_owned();

    // Someone else's claim is not theirs to know about: 404, not 403.
    let mut stranger = Client::new(router.clone());
    stranger.register("stranger@example.test").await;
    assert_eq!(
        stranger
            .post(&format!("/v1/me/claims/{claim_id}/withdraw"), json!({}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Under auto-approval no claim is ever pending, so withdrawal of one's
    // own (now approved) claim is a conflict, not a way to un-own a listing.
    assert_eq!(
        claimant
            .post(&format!("/v1/me/claims/{claim_id}/withdraw"), json!({}))
            .await
            .status,
        StatusCode::CONFLICT
    );

    let mine = claimant.get("/v1/me/claims").await;
    assert_eq!(mine.json[0]["status"], "approved");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_second_claim_on_a_claimed_listing_is_refused(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let router = router(pool.clone());

    let owner = cm_core::new_id();
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::insert(
        &mut conn,
        owner,
        Some("owner@example.test"),
        "Owner",
        cm_db::repo::users::AccountType::Contractor,
    )
    .await
    .expect("user");
    drop(conn);
    common::force_claim(&pool, id, owner).await;

    let mut latecomer = Client::new(router);
    latecomer
        .register_contractor("latecomer@example.test")
        .await;
    let refused = latecomer
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review" }),
        )
        .await;

    assert_eq!(refused.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_auto_approval_is_auditable_end_to_end(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let router = router(pool.clone());

    let mut claimant = Client::new(router.clone());
    claimant.register_contractor("claimant@example.test").await;
    claimant
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review" }),
        )
        .await;

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .expect("audit");
    for expected in ["claim.opened", "claim.approved"] {
        assert!(
            actions.iter().any(|a| a == expected),
            "{expected} in {actions:?}"
        );
    }

    // The approval names itself as automatic and records what the badge
    // became, so "who approved this and why" is answerable later.
    let data: serde_json::Value =
        sqlx::query_scalar("SELECT data FROM audit_log WHERE action = 'claim.approved'")
            .fetch_one(&pool)
            .await
            .expect("row");
    assert_eq!(data["auto_approved"], true);
    assert_eq!(data["verified"], true);
    assert!(data["verification_reason"].is_string());

    // No verification check is fabricated: that table records checks somebody
    // actually performed, and nobody performed one here.
    let checks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM verification_checks WHERE contractor_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(checks, 0);
}
