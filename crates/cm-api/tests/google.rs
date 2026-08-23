//! Google sign-in through the HTTP surface.
//!
//! Runs in Firebase emulator mode, so the tokens are unsigned and the test does
//! not need Google to be reachable. Signature handling is covered separately in
//! `cm-auth`; what is proved here is the wiring and, above all, the account
//! resolution rule.

mod common;

use common::{router_with, Client, PASSWORD};
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

/// An emulator-shaped token: header, payload, empty signature.
fn token(google_subject: &str, email: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "sub": format!("firebase-{google_subject}"),
        "user_id": format!("firebase-{google_subject}"),
        "email": email,
        "email_verified": true,
        "auth_time": now - 30,
        "iat": now - 30,
        "exp": now + 3600,
        "iss": format!("https://securetoken.google.com/{PROJECT}"),
        "aud": PROJECT,
        "firebase": {
            "sign_in_provider": "google.com",
            "identities": { "google.com": [google_subject] }
        }
    });

    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{payload}.")
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_first_google_sign_in_creates_an_account_and_a_session(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let response = client
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("100000000000000000001", "marisol@example.test") }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["user"]["email"], "marisol@example.test");
    assert_eq!(client.get("/v1/me").await.status, StatusCode::OK);

    // The identity is keyed on Google's subject; the Firebase uid is recorded
    // but is not the key.
    let (subject, firebase_uid): (String, Option<String>) =
        sqlx::query_as("SELECT subject, firebase_uid FROM oauth_identities")
            .fetch_one(&pool)
            .await
            .expect("identity row");
    assert_eq!(subject, "100000000000000000001");
    assert_eq!(
        firebase_uid.as_deref(),
        Some("firebase-100000000000000000001")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_returning_google_user_gets_the_same_account(pool: PgPool) {
    let router = emulator_router(pool.clone());
    let id_token = token("100000000000000000001", "marisol@example.test");

    let first = Client::new(router.clone())
        .post("/v1/auth/google", json!({ "id_token": id_token }))
        .await;
    let id_token = token("100000000000000000001", "marisol@example.test");
    let second = Client::new(router)
        .post("/v1/auth/google", json!({ "id_token": id_token }))
        .await;

    assert_eq!(first.json["user"]["id"], second.json["user"]["id"]);

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(
        accounts, 1,
        "a returning identity must not create a second account"
    );
}

/// The rule the whole design turns on: an address is never used to find an
/// existing account.
#[sqlx::test(migrations = "../../migrations")]
async fn a_google_account_is_never_matched_to_an_existing_account_by_email(pool: PgPool) {
    let router = emulator_router(pool.clone());
    let shared = "marisol@example.test";

    // A password account already holds the address.
    let mut password_user = Client::new(router.clone());
    assert_eq!(
        password_user.register(shared).await.status,
        StatusCode::CREATED
    );

    // Someone signs in with a Google account bearing the same address.
    let response = Client::new(router)
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("999", shared) }),
        )
        .await;

    assert_eq!(
        response.status,
        StatusCode::CONFLICT,
        "the Google sign-in must not be given the existing account: {:?}",
        response.json
    );
    assert!(response.json["error"]["message"]
        .as_str()
        .expect("message")
        .contains("link"));

    // The password account is untouched and still works.
    assert_eq!(password_user.get("/v1/me").await.status, StatusCode::OK);
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(accounts, 1);
}

/// Two Google accounts sharing an address produce two of ours, not one.
#[sqlx::test(migrations = "../../migrations")]
async fn distinct_google_subjects_never_collapse_into_one_account(pool: PgPool) {
    let router = emulator_router(pool.clone());

    let first = Client::new(router.clone())
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("111", "one@example.test") }),
        )
        .await;
    let second = Client::new(router)
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("222", "two@example.test") }),
        )
        .await;

    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(second.status, StatusCode::OK);
    assert_ne!(first.json["user"]["id"], second.json["user"]["id"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn linking_requires_being_signed_in_and_is_one_per_provider(pool: PgPool) {
    let router = emulator_router(pool.clone());

    // Anonymous linking is refused before anything else happens.
    let anonymous = Client::new(router.clone())
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("777", "x@example.test") }),
        )
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let mut client = Client::new(router.clone());
    client.register("owner@example.test").await;

    let linked = client
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("777", "owner@example.test") }),
        )
        .await;
    assert_eq!(linked.status, StatusCode::NO_CONTENT, "{:?}", linked.json);

    // Now that Google account signs in and lands on the same account.
    let signed_in = Client::new(router.clone())
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("777", "owner@example.test") }),
        )
        .await;
    assert_eq!(signed_in.status, StatusCode::OK);
    assert_eq!(signed_in.json["user"]["email"], "owner@example.test");

    // A second Google identity on the same account is refused.
    let again = client
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("888", "other@example.test") }),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // And that identity cannot be claimed by a different account either.
    let mut other = Client::new(router);
    other.register("someone-else@example.test").await;
    let stolen = other
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("777", "owner@example.test") }),
        )
        .await;
    assert_eq!(stolen.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "../../migrations")]
async fn linking_is_csrf_protected_like_any_other_mutation(pool: PgPool) {
    let router = emulator_router(pool);

    let mut signed_in = Client::new(router.clone());
    signed_in.register("owner@example.test").await;
    let session = signed_in.session_cookie().expect("token").to_owned();

    let mut without = Client::new(router).without_csrf();
    without.set_session(&session);
    let response = without
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("777", "x@example.test") }),
        )
        .await;

    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../../migrations")]
async fn google_sign_in_reports_clearly_when_it_is_not_configured(pool: PgPool) {
    // The default test configuration names no Firebase project.
    let mut client = Client::new(common::router(pool));

    let response = client
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("111", "x@example.test") }),
        )
        .await;

    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.json["error"]["code"], "unavailable");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_password_account_can_still_use_its_password_after_linking(pool: PgPool) {
    let router = emulator_router(pool);

    let mut client = Client::new(router.clone());
    client.register("both@example.test").await;
    client
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("555", "both@example.test") }),
        )
        .await;

    let by_password = Client::new(router)
        .login("both@example.test", PASSWORD)
        .await;
    assert_eq!(by_password.status, StatusCode::OK);
}
