//! Federated sign-in through the HTTP surface.
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
///
/// `firebase_provider` is Firebase's own name for the provider — "google.com"
/// or "facebook.com" — and appears twice, exactly as it does in a real token.
fn provider_token(firebase_provider: &str, subject: &str, email: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "sub": format!("firebase-{subject}"),
        "user_id": format!("firebase-{subject}"),
        "email": email,
        "email_verified": true,
        "auth_time": now - 30,
        "iat": now - 30,
        "exp": now + 3600,
        "iss": format!("https://securetoken.google.com/{PROJECT}"),
        "aud": PROJECT,
        "firebase": {
            "sign_in_provider": firebase_provider,
            "identities": { firebase_provider: [subject] }
        }
    });

    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{payload}.")
}

fn token(google_subject: &str, email: &str) -> String {
    provider_token("google.com", google_subject, email)
}

fn facebook_token(app_scoped_id: &str, email: &str) -> String {
    provider_token("facebook.com", app_scoped_id, email)
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_first_google_sign_in_creates_an_account_and_a_session(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let response = client
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("100000000000000000001", "marisol@example.test"), "account_type": "homeowner" }),
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
        .post(
            "/v1/auth/google",
            json!({ "id_token": id_token, "account_type": "homeowner" }),
        )
        .await;
    let id_token = token("100000000000000000001", "marisol@example.test");
    let second = Client::new(router)
        .post(
            "/v1/auth/google",
            json!({ "id_token": id_token, "account_type": "homeowner" }),
        )
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
    assert_eq!(password_user.register(shared).await.status, StatusCode::OK);

    // Someone signs in with a Google account bearing the same address.
    let response = Client::new(router)
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("999", shared), "account_type": "homeowner" }),
        )
        .await;

    assert_eq!(
        response.status,
        StatusCode::CONFLICT,
        "the Google sign-in must not be given the existing account: {:?}",
        response.json
    );
    // Asserts the property rather than a particular word: the refusal has to
    // tell the person what to do instead, or a 409 here is a dead end. Pinning
    // the exact wording made this fail when the copy was corrected to stop
    // naming an account-settings page that does not exist.
    let message = response.json["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("email"),
        "the refusal must point at the account they already have: {message}"
    );

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
            json!({ "id_token": token("111", "one@example.test"), "account_type": "homeowner" }),
        )
        .await;
    let second = Client::new(router)
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("222", "two@example.test"), "account_type": "homeowner" }),
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
            json!({ "id_token": token("777", "x@example.test"), "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let mut client = Client::new(router.clone());
    client.register("owner@example.test").await;

    let linked = client
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("777", "owner@example.test"), "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(linked.status, StatusCode::NO_CONTENT, "{:?}", linked.json);

    // Now that Google account signs in and lands on the same account.
    let signed_in = Client::new(router.clone())
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("777", "owner@example.test"), "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(signed_in.status, StatusCode::OK);
    assert_eq!(signed_in.json["user"]["email"], "owner@example.test");

    // A second Google identity on the same account is refused.
    let again = client
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("888", "other@example.test"), "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // And that identity cannot be claimed by a different account either.
    let mut other = Client::new(router);
    other.register("someone-else@example.test").await;
    let stolen = other
        .post(
            "/v1/auth/link/google",
            json!({ "id_token": token("777", "owner@example.test"), "account_type": "homeowner" }),
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
            json!({ "id_token": token("777", "x@example.test"), "account_type": "homeowner" }),
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
            json!({ "id_token": token("111", "x@example.test"), "account_type": "homeowner" }),
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
            json!({ "id_token": token("555", "both@example.test"), "account_type": "homeowner" }),
        )
        .await;

    let by_password = Client::new(router)
        .login("both@example.test", PASSWORD)
        .await;
    assert_eq!(by_password.status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_first_facebook_sign_in_creates_an_account_and_a_session(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let response = client
        .post(
            "/v1/auth/facebook",
            json!({ "id_token": facebook_token("fb-app-scoped-1", "dana@example.test"), "account_type": "homeowner" }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["user"]["email"], "dana@example.test");
    assert_eq!(client.get("/v1/me").await.status, StatusCode::OK);

    // Stored under its own provider, keyed on Meta's app-scoped id.
    let (provider, subject): (String, String) =
        sqlx::query_as("SELECT provider, subject FROM oauth_identities")
            .fetch_one(&pool)
            .await
            .expect("identity row");
    assert_eq!(provider, "facebook");
    assert_eq!(subject, "fb-app-scoped-1");
}

/// Each endpoint accepts only its own provider's tokens.
///
/// The route fixes the provider; the token never nominates it. This is what
/// stops a linked Firebase user — one uid carrying several identities, which is
/// what email-based linking produces — from turning control of one provider
/// into control of the account behind another.
#[sqlx::test(migrations = "../../migrations")]
async fn each_endpoint_refuses_the_other_providers_token(pool: PgPool) {
    let router = emulator_router(pool.clone());

    let at_google = Client::new(router.clone())
        .post(
            "/v1/auth/google",
            json!({ "id_token": facebook_token("fb-app-scoped-1", "dana@example.test"), "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(
        at_google.status,
        StatusCode::BAD_REQUEST,
        "a Facebook token must not be accepted as a Google sign-in"
    );

    let at_facebook = Client::new(router)
        .post(
            "/v1/auth/facebook",
            json!({ "id_token": token("100000000000000000001", "marisol@example.test"), "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(
        at_facebook.status,
        StatusCode::BAD_REQUEST,
        "and the mirror image"
    );

    let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_identities")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(identities, 0, "neither refusal may leave an account behind");
}

/// One account may hold one identity per provider, and only by linking while
/// signed in. Arriving at the second provider cold is a new account — or, when
/// the address is taken, a refusal that says what to do.
#[sqlx::test(migrations = "../../migrations")]
async fn a_shared_email_across_providers_is_a_conflict_not_a_merge(pool: PgPool) {
    let router = emulator_router(pool.clone());
    let email = "shared@example.test";

    let google = Client::new(router.clone())
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("g-1", email), "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(google.status, StatusCode::OK, "{:?}", google.json);

    let facebook = Client::new(router)
        .post(
            "/v1/auth/facebook",
            json!({ "id_token": facebook_token("fb-1", email), "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(
        facebook.status,
        StatusCode::CONFLICT,
        "a shared address must never merge two providers into one account"
    );
    assert!(
        facebook.json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Facebook"),
        "the message names the provider the user just tried: {:?}",
        facebook.json
    );

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(accounts, 1);
}

/// Facebook accounts can carry no email at all — a phone-number signup, or a
/// declined email permission. There is nowhere to put such a user yet, so the
/// refusal has to say so rather than fail obscurely.
#[sqlx::test(migrations = "../../migrations")]
async fn a_facebook_token_without_an_email_is_refused_with_advice(pool: PgPool) {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "sub": "firebase-fb-2",
        "user_id": "firebase-fb-2",
        "auth_time": now - 30,
        "iat": now - 30,
        "exp": now + 3600,
        "iss": format!("https://securetoken.google.com/{PROJECT}"),
        "aud": PROJECT,
        "firebase": {
            "sign_in_provider": "facebook.com",
            "identities": { "facebook.com": ["fb-2"] }
        }
    });
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let id_token = format!("{header}.{}.", URL_SAFE_NO_PAD.encode(claims.to_string()));

    let response = Client::new(emulator_router(pool.clone()))
        .post(
            "/v1/auth/facebook",
            json!({ "id_token": id_token, "account_type": "homeowner" }),
        )
        .await;

    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        response.json
    );
    assert!(
        response.json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("email address"),
        "the message has to name the problem: {:?}",
        response.json
    );

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(accounts, 0);
}

/// Signing up with a provider button creates the side the person chose.
///
/// The alternative — defaulting to homeowner because a token cannot say —
/// permanently traps every contractor who used a provider button, since an
/// account never changes sides. There is no route out of it except abandoning
/// the account, and nothing in the product would explain why.
#[sqlx::test(migrations = "../../migrations")]
async fn federated_sign_up_creates_the_side_the_person_chose(pool: PgPool) {
    let router = emulator_router(pool.clone());

    for (subject, email, chosen) in [
        ("choose-1", "owner@example.test", "homeowner"),
        ("choose-2", "pro@example.test", "contractor"),
    ] {
        let response = Client::new(router.clone())
            .post(
                "/v1/auth/google",
                json!({ "id_token": token(subject, email), "account_type": chosen }),
            )
            .await;

        assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
        assert_eq!(response.json["user"]["account_type"], chosen);
    }

    let contractors: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE account_type = 'contractor'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(contractors, 1, "the contractor was created as a contractor");
}

/// Arriving with no account and no chosen side is refused, not guessed at.
///
/// This is what the sign-in page produces: it sends no account type, because
/// someone signing in is expected to already have an account. Being told to go
/// and choose is the right outcome — being assigned a side at random is not.
#[sqlx::test(migrations = "../../migrations")]
async fn federated_sign_in_without_an_account_refuses_rather_than_guessing(pool: PgPool) {
    let response = Client::new(emulator_router(pool.clone()))
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("no-choice", "nobody@example.test") }),
        )
        .await;

    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        response.json
    );
    let message = response.json["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("homeowner"),
        "the message names the choice: {message}"
    );
    assert!(
        message.contains("cannot be changed"),
        "and says it is permanent: {message}"
    );

    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(users, 0, "nothing was created");
}

/// The field describes a NEW account and can never re-type an existing one.
///
/// An account not being able to change sides is the rule this product is built
/// on, so a field that could flip it through a sign-in endpoint would be the
/// most direct way to break that rule.
#[sqlx::test(migrations = "../../migrations")]
async fn the_account_type_field_cannot_re_type_an_existing_account(pool: PgPool) {
    let router = emulator_router(pool.clone());

    let created = Client::new(router.clone())
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("stable-1", "stays@example.test"),
                    "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{:?}", created.json);
    assert_eq!(created.json["user"]["account_type"], "homeowner");

    // The same identity returning, now claiming the other side.
    let returning = Client::new(router)
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("stable-1", "stays@example.test"),
                    "account_type": "contractor" }),
        )
        .await;
    assert_eq!(returning.status, StatusCode::OK, "{:?}", returning.json);
    assert_eq!(
        returning.json["user"]["account_type"], "homeowner",
        "a returning identity keeps its side, whatever the request claims"
    );

    let stored: String = sqlx::query_scalar("SELECT account_type FROM users")
        .fetch_one(&pool)
        .await
        .expect("account type");
    assert_eq!(stored, "homeowner");
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_unknown_account_type_is_refused_by_name(pool: PgPool) {
    let response = Client::new(emulator_router(pool.clone()))
        .post(
            "/v1/auth/google",
            json!({ "id_token": token("bad-type", "x@example.test"),
                    "account_type": "landlord" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.json["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("homeowner"));
}
