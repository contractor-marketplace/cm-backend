//! Proving control of an email address, after the fact.
//!
//! A federated account never travels the login-code path that verifies
//! everyone else, so its address — the copy a provider popup showed, or
//! nothing at all — needs its own proof: a code to the address, spent on the
//! account page. Job alerts wait for that proof, because mailing an unproved
//! address is sending digests to whoever the browser claimed.

mod common;

#[allow(unused_imports)]
use common::seed_jobs as _seed_jobs;
use common::{router_with, Client};
use http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

const PROJECT: &str = "cm-test-project";

fn emulator_router(pool: PgPool) -> axum::Router {
    router_with(
        pool,
        &[
            ("FIREBASE_PROJECT_ID", PROJECT),
            ("FIREBASE_AUTH_EMULATOR_HOST", "127.0.0.1:9099"),
        ],
    )
}

/// A production-shaped provider token: no top-level email claim.
fn provider_token(subject: &str, identities_email: Option<&str>) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let now = chrono::Utc::now().timestamp();
    let mut identities = json!({ "google.com": [subject] });
    if let Some(email) = identities_email {
        identities["email"] = json!([email]);
    }
    let claims = json!({
        "sub": format!("firebase-{subject}"),
        "user_id": format!("firebase-{subject}"),
        "auth_time": now - 30,
        "iat": now - 30,
        "exp": now + 3600,
        "iss": format!("https://securetoken.google.com/{PROJECT}"),
        "aud": PROJECT,
        "firebase": { "sign_in_provider": "google.com", "identities": identities }
    });
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    format!("{header}.{}.", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

/// Sign a fresh federated account in and hand back its client.
async fn federated_client(router: axum::Router, subject: &str, email: Option<&str>) -> Client {
    let mut client = Client::new(router);
    let mut body = json!({
        "id_token": provider_token(subject, None),
        "account_type": "homeowner"
    });
    if let Some(address) = email {
        body["email"] = json!(address);
    }
    let response = client.post("/v1/auth/google", body).await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    client
}

/// The newest code queued for an address, read straight from the outbox.
async fn latest_code(pool: &PgPool, recipient: &str) -> String {
    let subject: String = sqlx::query_scalar(
        "SELECT subject FROM email_outbox WHERE recipient = $1 AND kind = 'email_verify' \
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(recipient)
    .fetch_one(pool)
    .await
    .expect("a queued verification email");
    subject
        .split_whitespace()
        .next()
        .expect("the subject leads with the code")
        .to_owned()
}

/// The whole journey for the account this exists for: created with a provider,
/// address unproved, then confirmed — and only then eligible for job alerts.
#[sqlx::test(migrations = "../../migrations")]
async fn a_federated_account_confirms_the_address_it_arrived_with(pool: PgPool) {
    let mut client = federated_client(
        emulator_router(pool.clone()),
        "subject-1",
        Some("marisol@example.test"),
    )
    .await;

    // Unproved on arrival.
    let me = client.get("/v1/me").await;
    assert_eq!(me.json["user"]["email_verified"], false, "{:?}", me.json);

    // Ask for the code; none was sent to a body-less request's address twice.
    let requested = client.post("/v1/me/email", json!({})).await;
    assert_eq!(
        requested.status,
        StatusCode::ACCEPTED,
        "{:?}",
        requested.json
    );
    assert_eq!(requested.json["email"], "marisol@example.test");

    let code = latest_code(&pool, "marisol@example.test").await;
    let confirmed = client
        .post(
            "/v1/me/email/verify",
            json!({
                "challenge_id": requested.json["challenge_id"],
                "code": code,
                "email": "marisol@example.test"
            }),
        )
        .await;

    assert_eq!(confirmed.status, StatusCode::OK, "{:?}", confirmed.json);
    assert_eq!(confirmed.json["email_verified"], true);

    let verified_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT email_verified_at FROM users")
            .fetch_one(&pool)
            .await
            .expect("user row");
    assert!(verified_at.is_some());
}

/// An account with no address at all adds one. The address is written and
/// verified in a single step — there is never a stored-but-unproved state on
/// this path.
#[sqlx::test(migrations = "../../migrations")]
async fn an_account_without_an_email_adds_one(pool: PgPool) {
    let mut client = federated_client(emulator_router(pool.clone()), "subject-2", None).await;

    let me = client.get("/v1/me").await;
    assert_eq!(me.json["user"]["email"], serde_json::Value::Null);

    // Asking to confirm with nothing on file and nothing supplied is refused
    // with something actionable.
    let empty = client.post("/v1/me/email", json!({})).await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{:?}", empty.json);

    let requested = client
        .post("/v1/me/email", json!({ "email": "new@example.test" }))
        .await;
    assert_eq!(
        requested.status,
        StatusCode::ACCEPTED,
        "{:?}",
        requested.json
    );

    let code = latest_code(&pool, "new@example.test").await;
    let confirmed = client
        .post(
            "/v1/me/email/verify",
            json!({
                "challenge_id": requested.json["challenge_id"],
                "code": code,
                "email": "new@example.test"
            }),
        )
        .await;

    assert_eq!(confirmed.status, StatusCode::OK, "{:?}", confirmed.json);
    assert_eq!(confirmed.json["email"], "new@example.test");
    assert_eq!(confirmed.json["email_verified"], true);
}

/// The address is bound into the code's digest: the right code presented for a
/// different address is a wrong code, not a different target.
#[sqlx::test(migrations = "../../migrations")]
async fn the_code_only_proves_the_address_it_was_sent_to(pool: PgPool) {
    let mut client = federated_client(emulator_router(pool.clone()), "subject-3", None).await;

    let requested = client
        .post("/v1/me/email", json!({ "email": "real@example.test" }))
        .await;
    let code = latest_code(&pool, "real@example.test").await;

    let swapped = client
        .post(
            "/v1/me/email/verify",
            json!({
                "challenge_id": requested.json["challenge_id"],
                "code": code,
                "email": "attacker@example.test"
            }),
        )
        .await;

    assert_eq!(
        swapped.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        swapped.json
    );

    let email: Option<String> = sqlx::query_scalar("SELECT email FROM users")
        .fetch_one(&pool)
        .await
        .expect("user row");
    assert_eq!(email, None, "the swap must not have written anything");
}

/// A confirmed address that belongs to another account is the standard 409 —
/// the collision tripwire fires wherever an address is known, this path
/// included.
#[sqlx::test(migrations = "../../migrations")]
async fn confirming_an_address_another_account_holds_is_a_conflict(pool: PgPool) {
    let router = emulator_router(pool.clone());

    let mut holder = Client::new(router.clone());
    assert_eq!(
        holder.register("taken@example.test").await.status,
        StatusCode::OK
    );

    let mut client = federated_client(router, "subject-4", None).await;
    let requested = client
        .post("/v1/me/email", json!({ "email": "taken@example.test" }))
        .await;
    assert_eq!(
        requested.status,
        StatusCode::ACCEPTED,
        "{:?}",
        requested.json
    );

    let code = latest_code(&pool, "taken@example.test").await;
    let confirmed = client
        .post(
            "/v1/me/email/verify",
            json!({
                "challenge_id": requested.json["challenge_id"],
                "code": code,
                "email": "taken@example.test"
            }),
        )
        .await;

    assert_eq!(
        confirmed.status,
        StatusCode::CONFLICT,
        "{:?}",
        confirmed.json
    );
}

/// Job alerts deliver only to proved addresses. The gate is the join itself,
/// so an unproved account's saved search matches nothing and the same search
/// starts matching the moment the address is confirmed.
#[sqlx::test(migrations = "../../migrations")]
async fn job_alerts_wait_for_the_proof(pool: PgPool) {
    let mut client = federated_client(
        emulator_router(pool.clone()),
        "subject-5",
        Some("marisol@example.test"),
    )
    .await;

    // A saved search with notifications on, straight into the repo — the HTTP
    // surface for saved searches is covered elsewhere.
    let user_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(&pool)
        .await
        .expect("user");
    sqlx::query(
        "INSERT INTO saved_searches (id, user_id, name, notify) VALUES ($1, $2, 'anything', true)",
    )
    .bind(cm_core::new_id())
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("saved search");

    // A live job by somebody else, through the shared seeder so the fixture
    // satisfies the intake constraints without restating them.
    let poster: uuid::Uuid = cm_core::new_id();
    cm_db::repo::users::insert(
        &mut pool.acquire().await.expect("conn"),
        poster,
        Some("poster@example.test"),
        "Poster",
        cm_db::repo::users::AccountType::Homeowner,
    )
    .await
    .expect("poster");
    let job_id = common::seed_jobs(&pool, poster, 1, "90026").await[0];

    let mut conn = pool.acquire().await.expect("conn");
    let before = cm_db::repo::saved_searches::matches_for_jobs(&mut conn, &[job_id])
        .await
        .expect("matches");
    assert!(
        before.is_empty(),
        "an unproved address must receive nothing: {before:?}"
    );

    // Prove it, then the same search matches.
    let requested = client.post("/v1/me/email", json!({})).await;
    let code = latest_code(&pool, "marisol@example.test").await;
    let confirmed = client
        .post(
            "/v1/me/email/verify",
            json!({
                "challenge_id": requested.json["challenge_id"],
                "code": code,
                "email": "marisol@example.test"
            }),
        )
        .await;
    assert_eq!(confirmed.status, StatusCode::OK, "{:?}", confirmed.json);

    let after = cm_db::repo::saved_searches::matches_for_jobs(&mut conn, &[job_id])
        .await
        .expect("matches");
    assert_eq!(after.len(), 1, "the proof is the only thing that changed");
}
