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

/// The token production actually mints.
///
/// With account linking off — the console mode this product requires — Firebase
/// strips the top-level `email` claim from OAuth tokens. When it kept an
/// address at all, it rides in the identities map's `"email"` slot; sometimes
/// there is none anywhere. The sign-up outage happened because only the
/// always-present builder above existed, so no test could mint what production
/// mints.
fn production_token(
    firebase_provider: &str,
    subject: &str,
    identities_email: Option<&str>,
) -> String {
    production_token_named(firebase_provider, subject, identities_email, None)
}

/// The same shape with the `name` claim Google attaches to essentially every
/// token: the person's profile name, signed by the provider.
fn production_token_named(
    firebase_provider: &str,
    subject: &str,
    identities_email: Option<&str>,
    name: Option<&str>,
) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let now = chrono::Utc::now().timestamp();
    let mut identities = json!({ firebase_provider: [subject] });
    if let Some(email) = identities_email {
        identities["email"] = json!([email]);
    }
    let mut claims = json!({
        "sub": format!("firebase-{subject}"),
        "user_id": format!("firebase-{subject}"),
        "auth_time": now - 30,
        "iat": now - 30,
        "exp": now + 3600,
        "iss": format!("https://securetoken.google.com/{PROJECT}"),
        "aud": PROJECT,
        "firebase": {
            "sign_in_provider": firebase_provider,
            "identities": identities
        }
    });
    if let Some(name) = name {
        claims["name"] = json!(name);
    }

    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{payload}.")
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
    // This is exactly the collision whose other side is NOT a password
    // account — the address belongs to a Google-created one. The advice must
    // therefore stay method-agnostic: pointing at "your password" here sends
    // the person to a credential that does not exist.
    let message = facebook.json["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("email") && !message.contains("password"),
        "the refusal points at the existing account without assuming how it \
         signs in: {message}"
    );

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(accounts, 1);
}

/// A Facebook account with no email anywhere still gets in.
///
/// Registered with a phone number, or the email permission declined: there is
/// genuinely no address, in the token or the popup. Since 0035 that person is
/// admitted — the account keys on the provider subject, contact happens in the
/// app, and an address can be added from the account page when they want
/// notifications. This used to be a 400 telling them to go use the email form.
#[sqlx::test(migrations = "../../migrations")]
async fn a_facebook_account_without_an_email_still_gets_in(pool: PgPool) {
    let response = Client::new(emulator_router(pool.clone()))
        .post(
            "/v1/auth/facebook",
            json!({
                "id_token": production_token("facebook.com", "fb-2", None),
                "account_type": "homeowner"
            }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["user"]["email"], serde_json::Value::Null);
    assert_eq!(response.json["user"]["email_verified"], false);

    let (email, display_name): (Option<String>, String) =
        sqlx::query_as("SELECT email, display_name FROM users")
            .fetch_one(&pool)
            .await
            .expect("user row");
    assert_eq!(email, None);
    assert_eq!(
        display_name, "Facebook user",
        "a placeholder name beats blocking the person for the lack of one"
    );
}

/// Two email-less accounts coexist: the unique index is on the normalised
/// address, and NULLs never collide. Without this property, the second
/// no-email sign-up ever would 409 against the first.
#[sqlx::test(migrations = "../../migrations")]
async fn two_accounts_without_emails_do_not_collide(pool: PgPool) {
    let router = emulator_router(pool.clone());

    for subject in ["fb-a", "fb-b"] {
        let response = Client::new(router.clone())
            .post(
                "/v1/auth/facebook",
                json!({
                    "id_token": production_token("facebook.com", subject, None),
                    "account_type": "homeowner"
                }),
            )
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{subject}: {:?}",
            response.json
        );
    }

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(accounts, 2);
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

/* ── The production token shape ─────────────────────────────────────────────
Every test above minted tokens with a top-level email claim, which is why a
day of green tests coexisted with a total sign-up outage. These four mint
what the console mode actually produces. ─────────────────────────────────*/

/// No top-level claim, address in the identities slot: sign-up works, and the
/// address is contact info, not proof — nothing marked verified.
#[sqlx::test(migrations = "../../migrations")]
async fn a_production_shaped_token_signs_up_via_the_identities_slot(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let response = client
        .post(
            "/v1/auth/google",
            json!({
                "id_token": production_token("google.com", "prod-subject-1", Some("marisol@example.test")),
                "account_type": "homeowner"
            }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["user"]["email"], "marisol@example.test");
    assert_eq!(
        response.json["user"]["email_verified"], false,
        "an identities-slot address is unproved: {:?}",
        response.json
    );
}

/// No address in the token at all — the popup's copy, forwarded by the
/// browser, is accepted at exactly the trust level of a typed one.
#[sqlx::test(migrations = "../../migrations")]
async fn a_token_with_no_email_anywhere_accepts_the_popups_copy(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let response = client
        .post(
            "/v1/auth/google",
            json!({
                "id_token": production_token("google.com", "prod-subject-2", None),
                "account_type": "contractor",
                "email": "marisol@example.test"
            }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["user"]["email"], "marisol@example.test");
    assert_eq!(response.json["user"]["email_verified"], false);
    assert_eq!(response.json["user"]["account_type"], "contractor");

    let verified_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT email_verified_at FROM users")
            .fetch_one(&pool)
            .await
            .expect("user row");
    assert!(
        verified_at.is_none(),
        "a client-supplied address must not verify itself"
    );
}

/// The token outranks the browser: when both carry an address, the verified
/// token's wins and the client copy is ignored.
#[sqlx::test(migrations = "../../migrations")]
async fn the_tokens_address_outranks_the_browsers(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let response = client
        .post(
            "/v1/auth/google",
            json!({
                "id_token": production_token("google.com", "prod-subject-3", Some("token@example.test")),
                "account_type": "homeowner",
                "email": "attacker-chosen@example.test"
            }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["user"]["email"], "token@example.test");
}

/// A client-supplied address that collides with an existing account is a
/// conflict, same as everywhere else — and the advice no longer assumes the
/// other account has a password.
#[sqlx::test(migrations = "../../migrations")]
async fn a_client_address_colliding_with_an_existing_account_is_refused(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let registered = client
        .post(
            "/v1/auth/register",
            json!({
                "email": "marisol@example.test",
                "display_name": "Marisol",
                "password": PASSWORD,
                "account_type": "homeowner"
            }),
        )
        .await;
    assert_eq!(
        registered.status,
        StatusCode::ACCEPTED,
        "{:?}",
        registered.json
    );

    let response = client
        .post(
            "/v1/auth/google",
            json!({
                "id_token": production_token("google.com", "prod-subject-4", None),
                "account_type": "homeowner",
                "email": "MARISOL@Example.TEST"
            }),
        )
        .await;

    assert_eq!(response.status, StatusCode::CONFLICT, "{:?}", response.json);
    let message = response.json["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !message.contains("password"),
        "the advice must not assume the colliding account has one: {message}"
    );
}

/// A returning identity is a sign-in, and a sign-in changes nothing: whatever
/// address rides in the body is ignored outright.
#[sqlx::test(migrations = "../../migrations")]
async fn a_client_address_is_ignored_for_a_returning_identity(pool: PgPool) {
    let router = emulator_router(pool.clone());
    let id_token = production_token("google.com", "prod-subject-5", Some("marisol@example.test"));

    let first = Client::new(router.clone())
        .post(
            "/v1/auth/google",
            json!({ "id_token": id_token, "account_type": "homeowner" }),
        )
        .await;
    assert_eq!(first.status, StatusCode::OK, "{:?}", first.json);

    let second = Client::new(router)
        .post(
            "/v1/auth/google",
            json!({ "id_token": id_token, "email": "other@example.test" }),
        )
        .await;

    assert_eq!(second.status, StatusCode::OK, "{:?}", second.json);
    assert_eq!(
        second.json["user"]["email"], "marisol@example.test",
        "a sign-in must not rewrite the account's address"
    );
}

/// The account page can say which sign-ins are attached: /v1/me names the
/// connected providers, and only the names — subjects stay server-side.
#[sqlx::test(migrations = "../../migrations")]
async fn the_account_page_knows_which_providers_are_connected(pool: PgPool) {
    let router = emulator_router(pool.clone());

    let mut federated = Client::new(router.clone());
    let signed_in = federated
        .post(
            "/v1/auth/google",
            json!({
                "id_token": production_token("google.com", "prov-1", Some("marisol@example.test")),
                "account_type": "homeowner"
            }),
        )
        .await;
    assert_eq!(signed_in.status, StatusCode::OK, "{:?}", signed_in.json);

    let me = federated.get("/v1/me").await;
    assert_eq!(
        me.json["connected_providers"],
        json!(["google"]),
        "{:?}",
        me.json
    );

    // A password account reports none.
    let mut password_user = Client::new(router);
    assert_eq!(
        password_user.register("plain@example.test").await.status,
        StatusCode::OK
    );
    let me = password_user.get("/v1/me").await;
    assert_eq!(me.json["connected_providers"], json!([]), "{:?}", me.json);
}

/// "Forgot password" doubles as "set a first password": a federated account
/// has no credential row, and the reset upsert is exactly how it gets one —
/// after which the email form works alongside the provider button.
#[sqlx::test(migrations = "../../migrations")]
async fn a_reset_gives_a_federated_account_its_first_password(pool: PgPool) {
    let router = emulator_router(pool.clone());
    let mut client = Client::new(router.clone());

    let signed_in = client
        .post(
            "/v1/auth/google",
            json!({
                "id_token": production_token("google.com", "prov-2", Some("marisol@example.test")),
                "account_type": "homeowner"
            }),
        )
        .await;
    assert_eq!(signed_in.status, StatusCode::OK, "{:?}", signed_in.json);

    // The email login path knows nothing of this account's password — there
    // is none — and answers its uniform refusal.
    let mut visitor = Client::new(router.clone());
    let refused = visitor
        .post(
            "/v1/auth/login",
            json!({ "email": "marisol@example.test", "password": PASSWORD }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    // Reset issues a token to the address on file; spending it upserts the
    // first credential.
    let requested = visitor
        .post(
            "/v1/auth/password-reset/request",
            json!({ "email": "marisol@example.test" }),
        )
        .await;
    assert_eq!(
        requested.status,
        StatusCode::NO_CONTENT,
        "{:?}",
        requested.json
    );

    let token = {
        let mail = visitor
            .get("/__test/latest-email?to=marisol@example.test")
            .await;
        let body = mail.json["body_text"].as_str().expect("a reset email");
        let start = body.find("token=").expect("a link") + "token=".len();
        body[start..]
            .split_whitespace()
            .next()
            .expect("a token")
            .to_owned()
    };
    let confirmed = visitor
        .post(
            "/v1/auth/password-reset/confirm",
            json!({ "token": token, "new_password": PASSWORD }),
        )
        .await;
    assert_eq!(
        confirmed.status,
        StatusCode::NO_CONTENT,
        "{:?}",
        confirmed.json
    );

    // Both doors now open: the password works, and the provider still does.
    let by_password = visitor
        .post(
            "/v1/auth/login",
            json!({ "email": "marisol@example.test", "password": PASSWORD }),
        )
        .await;
    assert!(
        by_password.status == StatusCode::OK || by_password.status == StatusCode::ACCEPTED,
        "a session or a code challenge, never a refusal: {:?}",
        by_password.json
    );
}

/// The account is named after its person, not their address.
///
/// Google signs the person's profile name into the token; losing it meant an
/// account created from marisol@… greeted its owner as "marisol", and one
/// created with no address at all as "Google user". The token's claim also
/// outranks the browser's copy — same precedence the email takes, and for the
/// same reason: what the provider signed beats what the client asserts.
#[sqlx::test(migrations = "../../migrations")]
async fn the_tokens_name_claim_becomes_the_display_name(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let response = client
        .post(
            "/v1/auth/google",
            json!({
                "id_token": production_token_named(
                    "google.com",
                    "named-subject-1",
                    Some("marisol@example.test"),
                    Some("Marisol Vega"),
                ),
                "account_type": "homeowner",
                "display_name": "Browser Asserted"
            }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(
        response.json["user"]["display_name"], "Marisol Vega",
        "the signed name wins: {:?}",
        response.json
    );
}

/// With no name in the token, the popup's copy fills in — the same standing as
/// a name typed into the email form, which is any string anybody likes. Only
/// with no name anywhere does the address's local part stand in.
#[sqlx::test(migrations = "../../migrations")]
async fn the_popups_name_fills_in_when_the_token_carries_none(pool: PgPool) {
    let mut client = Client::new(emulator_router(pool.clone()));

    let response = client
        .post(
            "/v1/auth/google",
            json!({
                "id_token": production_token("google.com", "named-subject-2", None),
                "account_type": "homeowner",
                "email": "marisol@example.test",
                "display_name": "Marisol Vega"
            }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(
        response.json["user"]["display_name"], "Marisol Vega",
        "{:?}",
        response.json
    );
}
