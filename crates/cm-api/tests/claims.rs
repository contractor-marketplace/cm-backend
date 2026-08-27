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
    assert_eq!(opened.json["status"], "pending");
    let claim_id = opened.json["id"].as_str().expect("id").to_owned();

    // Not verified yet: a claim nobody has decided is an assertion.
    let mut anyone = Client::new(router.clone());
    let before = anyone.get(&format!("/v1/contractors/{id}")).await;
    assert_eq!(before.json["verified"], false);
    assert_eq!(before.json["is_claimed"], false);

    let mut admin = Client::new(router.clone());
    admin.register("admin@example.test").await;
    make_admin(&pool, "admin@example.test").await;
    // The role only takes effect on the next request, which re-reads it.
    let queue = admin.get("/v1/admin/claims").await;
    assert_eq!(queue.status, StatusCode::OK, "{:?}", queue.json);
    assert_eq!(queue.json.as_array().expect("array").len(), 1);

    let decided = admin
        .post(
            &format!("/v1/admin/claims/{claim_id}/decide"),
            json!({ "approve": true, "note": "licence and phone check passed" }),
        )
        .await;
    assert_eq!(decided.status, StatusCode::OK, "{:?}", decided.json);
    assert_eq!(decided.json["verified"], true);
    assert!(decided.json["verification_reason"]
        .as_str()
        .expect("reason")
        .contains("1047382"));

    // The claim in the response must reflect the decision that was just made.
    // This asserted nothing until it was added, and the endpoint was returning
    // the claim as it stood BEFORE the decision — so every approval and every
    // rejection reported back as still `pending` with no `decided_at`, and a
    // moderator's client would have shown that nothing happened.
    assert_eq!(
        decided.json["claim"]["status"], "approved",
        "the decision response returned a stale claim: {:?}",
        decided.json["claim"]
    );
    assert!(
        !decided.json["claim"]["decided_at"].is_null(),
        "a decided claim must carry the time it was decided"
    );
    assert_eq!(
        decided.json["claim"]["decision_note"],
        "licence and phone check passed"
    );

    let after = anyone.get(&format!("/v1/contractors/{id}")).await;
    assert_eq!(after.json["verified"], true);
    assert_eq!(after.json["is_claimed"], true);

    // The claimant now holds the contractor role.
    let me = claimant.get("/v1/me").await;
    assert_eq!(me.json["roles"], json!(["contractor"]));
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
    let claim_id = opened.json["id"].as_str().expect("id").to_owned();

    let mut admin = Client::new(router.clone());
    admin.register("admin@example.test").await;
    make_admin(&pool, "admin@example.test").await;

    let decided = admin
        .post(
            &format!("/v1/admin/claims/{claim_id}/decide"),
            json!({ "approve": true }),
        )
        .await;

    assert_eq!(decided.status, StatusCode::OK);
    assert_eq!(decided.json["verified"], false);
    assert!(decided.json["verification_reason"]
        .as_str()
        .expect("reason")
        .contains("expired"));
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
        "owner@example.test",
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
async fn two_simultaneous_approvals_produce_exactly_one_owner(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let router = router(pool.clone());

    // Two people claim the same listing.
    let mut first = Client::new(router.clone());
    first.register_contractor("first@example.test").await;
    let mut second = Client::new(router.clone());
    second.register_contractor("second@example.test").await;

    let a = first
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review" }),
        )
        .await;
    let b = second
        .post(
            &format!("/v1/contractors/{id}/claims"),
            json!({ "method": "manual_review" }),
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED);
    assert_eq!(
        b.status,
        StatusCode::CREATED,
        "two pending claims are allowed"
    );

    let claim_a = a.json["id"].as_str().expect("id").to_owned();
    let claim_b = b.json["id"].as_str().expect("id").to_owned();

    let mut admin = Client::new(router.clone());
    admin.register("admin@example.test").await;
    make_admin(&pool, "admin@example.test").await;
    admin.get("/v1/me").await;

    // Approve both at once.
    let approve = |claim: String| {
        let mut client = Client::new(router.clone());
        let session = admin.session_cookie().expect("session").to_owned();
        let csrf = admin.csrf_token().expect("csrf").to_owned();
        client.set_session(&session);
        client.set_csrf(&csrf);
        tokio::spawn(async move {
            client
                .post(
                    &format!("/v1/admin/claims/{claim}/decide"),
                    json!({ "approve": true }),
                )
                .await
                .status
        })
    };

    let (first_status, second_status) = tokio::join!(approve(claim_a), approve(claim_b));
    let statuses = [first_status.expect("join"), second_status.expect("join")];

    let successes = statuses.iter().filter(|s| s.is_success()).count();
    assert_eq!(successes, 1, "exactly one approval may win: {statuses:?}");
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
async fn a_claimant_may_withdraw_only_their_own_pending_claim(pool: PgPool) {
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

    assert_eq!(
        claimant
            .post(&format!("/v1/me/claims/{claim_id}/withdraw"), json!({}))
            .await
            .status,
        StatusCode::NO_CONTENT
    );

    // Withdrawing twice is a conflict, not a second withdrawal.
    assert_eq!(
        claimant
            .post(&format!("/v1/me/claims/{claim_id}/withdraw"), json!({}))
            .await
            .status,
        StatusCode::CONFLICT
    );

    let mine = claimant.get("/v1/me/claims").await;
    assert_eq!(mine.json[0]["status"], "withdrawn");
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
        "owner@example.test",
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
async fn a_decision_is_auditable_end_to_end(pool: PgPool) {
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

    let mut admin = Client::new(router);
    admin.register("admin@example.test").await;
    make_admin(&pool, "admin@example.test").await;
    admin.get("/v1/me").await;
    admin
        .post(
            &format!("/v1/admin/claims/{claim_id}/decide"),
            json!({ "approve": true }),
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

    // The evidence row names who decided and what the badge became.
    let data: serde_json::Value =
        sqlx::query_scalar("SELECT data FROM audit_log WHERE action = 'claim.approved'")
            .fetch_one(&pool)
            .await
            .expect("row");
    assert_eq!(data["verified"], true);
    assert!(data["verification_reason"].is_string());

    let checks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM verification_checks WHERE contractor_id = $1 AND kind = 'manual_review'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(checks, 1);
}
