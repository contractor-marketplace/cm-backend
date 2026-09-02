//! The emailed sign-in code: challenge, verify, resend, remembered devices.

mod common;

use common::{extract_code, router, Client, PASSWORD};
use http::StatusCode;
use sqlx::PgPool;

const EMAIL: &str = "marisol@example.test";

async fn raw_register(client: &mut Client, email: &str) -> common::TestResponse {
    client
        .post(
            "/v1/auth/register",
            serde_json::json!({
                "email": email,
                "display_name": "Test Person",
                "password": PASSWORD,
                "account_type": "homeowner",
            }),
        )
        .await
}

async fn latest_code(client: &mut Client, email: &str) -> String {
    let mail = client
        .get(&format!("/__test/latest-email?to={email}"))
        .await;
    extract_code(mail.json["body_text"].as_str().expect("an email"))
}

/// Registration creates the account and the challenge — and deliberately no
/// session: the code round-trip is the last step of registration.
#[sqlx::test(migrations = "../../migrations")]
async fn registering_returns_a_challenge_and_no_session(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));

    let response = raw_register(&mut client, EMAIL).await;
    assert_eq!(response.status, StatusCode::ACCEPTED, "{:?}", response.json);
    assert!(response.json["challenge_id"].is_string());
    assert_eq!(response.json["email"], EMAIL);
    assert!(
        response.set_cookies().is_empty(),
        "no cookie of any kind before the code: {:?}",
        response.set_cookies()
    );

    let me = client.get("/v1/me").await;
    assert_eq!(
        me.status,
        StatusCode::UNAUTHORIZED,
        "the account must not be signed in yet"
    );

    let verified: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT email_verified_at FROM users")
            .fetch_one(&pool)
            .await
            .expect("user row");
    assert!(verified.is_none(), "nothing is verified before the code");
}

/// The full two-step: the code from the email creates the session, marks this
/// browser remembered, and verifies the address in the same stroke.
#[sqlx::test(migrations = "../../migrations")]
async fn the_code_creates_a_session_and_verifies_the_address(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));

    let challenge = raw_register(&mut client, EMAIL).await;
    let code = latest_code(&mut client, EMAIL).await;

    let response = client
        .post(
            "/v1/auth/login/verify",
            serde_json::json!({
                "challenge_id": challenge.json["challenge_id"],
                "code": code,
            }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["user"]["email_verified"], true);
    assert!(
        response.cookie("__Host-cm_session").is_some(),
        "the session arrives with the verified code"
    );
    let device = response
        .cookie("__Host-cm_device")
        .expect("the browser is marked remembered");
    assert!(device.contains("; HttpOnly"), "{device}");
    assert!(device.contains("; Secure"), "{device}");

    let me = client.get("/v1/me").await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json["user"]["email_verified"], true);
}

/// A remembered browser presents its device cookie and never sees the code
/// step; a fresh browser with the same password always does.
#[sqlx::test(migrations = "../../migrations")]
async fn a_remembered_browser_logs_in_without_a_code(pool: PgPool) {
    let router = router(pool.clone());
    let mut client = Client::new(router.clone());
    client.register(EMAIL).await;

    // Sign out but keep the jar: the device cookie survives logout, the
    // session does not.
    client.post("/v1/auth/logout", serde_json::json!({})).await;
    let login = client.login(EMAIL, PASSWORD).await;
    assert_eq!(login.status, StatusCode::OK, "{:?}", login.json);

    // A brand-new browser gets challenged even with the right password.
    let mut fresh = Client::new(router);
    let response = fresh
        .post(
            "/v1/auth/login",
            serde_json::json!({ "email": EMAIL, "password": PASSWORD }),
        )
        .await;
    assert_eq!(response.status, StatusCode::ACCEPTED, "{:?}", response.json);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_forged_device_cookie_still_gets_challenged(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    client.register(EMAIL).await;
    client.clear_jar();

    // A cookie with the right shape and the wrong signature.
    let user_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(&pool)
        .await
        .expect("user id");
    let expiry = (chrono::Utc::now() + chrono::Duration::days(90)).timestamp();
    client.jar_insert(
        "__Host-cm_device",
        &format!("{user_id}.{expiry}.bm90LWEtcmVhbC1zaWduYXR1cmU"),
    );

    let response = client
        .post(
            "/v1/auth/login",
            serde_json::json!({ "email": EMAIL, "password": PASSWORD }),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::ACCEPTED,
        "a forged signature is an unremembered browser: {:?}",
        response.json
    );
}

/// The per-challenge attempt cap: five wrong guesses consume the challenge,
/// and the right code afterwards is refused with the same message.
#[sqlx::test(migrations = "../../migrations")]
async fn a_wrong_code_five_times_kills_the_challenge(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    let challenge = raw_register(&mut client, EMAIL).await;
    let challenge_id = challenge.json["challenge_id"].clone();
    let real_code = latest_code(&mut client, EMAIL).await;
    let wrong_code = if real_code == "000000" {
        "000001"
    } else {
        "000000"
    };

    for attempt in 0..5 {
        let response = client
            .post(
                "/v1/auth/login/verify",
                serde_json::json!({ "challenge_id": challenge_id, "code": wrong_code }),
            )
            .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "wrong guess {attempt}"
        );
        assert_eq!(response.json["error"]["code"], "invalid_request");
    }

    // The real code is now worthless: the challenge died with the fifth miss.
    let response = client
        .post(
            "/v1/auth/login/verify",
            serde_json::json!({ "challenge_id": challenge_id, "code": real_code }),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "a burnt challenge must not accept even the right code"
    );
}

/// Expired, consumed, and never-existed all read identically to a wrong code,
/// so the endpoint cannot be used to probe which challenges are live.
#[sqlx::test(migrations = "../../migrations")]
async fn an_expired_code_reads_the_same_as_a_wrong_one(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    let challenge = raw_register(&mut client, EMAIL).await;
    let code = latest_code(&mut client, EMAIL).await;

    sqlx::query("UPDATE auth_tokens SET expires_at = now() - interval '1 minute'")
        .execute(&pool)
        .await
        .expect("expire the challenge");

    let expired = client
        .post(
            "/v1/auth/login/verify",
            serde_json::json!({ "challenge_id": challenge.json["challenge_id"], "code": code }),
        )
        .await;
    let unknown = client
        .post(
            "/v1/auth/login/verify",
            serde_json::json!({ "challenge_id": uuid::Uuid::now_v7(), "code": "123456" }),
        )
        .await;

    assert_eq!(expired.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        expired.json, unknown.json,
        "expired and unknown must be indistinguishable"
    );
}

/// A resend is a fresh challenge, and the old code stops working the moment it
/// is issued — otherwise every resend would widen the guessing surface.
#[sqlx::test(migrations = "../../migrations")]
async fn a_new_code_invalidates_the_previous_one(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    let first = raw_register(&mut client, EMAIL).await;
    let old_code = latest_code(&mut client, EMAIL).await;

    let resent = client
        .post(
            "/v1/auth/login/resend",
            serde_json::json!({ "challenge_id": first.json["challenge_id"] }),
        )
        .await;
    assert_eq!(resent.status, StatusCode::ACCEPTED, "{:?}", resent.json);
    assert_ne!(
        resent.json["challenge_id"], first.json["challenge_id"],
        "a resend is a new challenge"
    );

    let stale = client
        .post(
            "/v1/auth/login/verify",
            serde_json::json!({ "challenge_id": first.json["challenge_id"], "code": old_code }),
        )
        .await;
    assert_eq!(
        stale.status,
        StatusCode::BAD_REQUEST,
        "the superseded challenge must be dead"
    );

    let new_code = latest_code(&mut client, EMAIL).await;
    let response = client
        .post(
            "/v1/auth/login/verify",
            serde_json::json!({ "challenge_id": resent.json["challenge_id"], "code": new_code }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
}

/// Issuing codes is bounded per account: someone holding a stolen password
/// cannot make a victim's inbox ring all day.
#[sqlx::test(migrations = "../../migrations")]
async fn code_issue_is_rate_limited_per_account(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    let challenge = raw_register(&mut client, EMAIL).await;
    let mut challenge_id = challenge.json["challenge_id"].clone();

    // Six resends reach the 6/hour ceiling. (Registration's own code rides
    // the register:ip limit instead, so it does not count here.)
    for _ in 0..6 {
        let resent = client
            .post(
                "/v1/auth/login/resend",
                serde_json::json!({ "challenge_id": challenge_id }),
            )
            .await;
        assert_eq!(resent.status, StatusCode::ACCEPTED, "{:?}", resent.json);
        challenge_id = resent.json["challenge_id"].clone();
    }

    let limited = client
        .post(
            "/v1/auth/login/resend",
            serde_json::json!({ "challenge_id": challenge_id }),
        )
        .await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
}
