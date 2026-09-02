//! The emailed password-reset link.

mod common;

use common::{router, Client, PASSWORD};
use http::StatusCode;
use sqlx::PgPool;

const EMAIL: &str = "marisol@example.test";
const NEW_PASSWORD: &str = "an entirely different long password";

/// The link out of the latest reset email, or rather its token.
async fn latest_reset_token(client: &mut Client, email: &str) -> String {
    let mail = client
        .get(&format!("/__test/latest-email?to={email}"))
        .await;
    let body = mail.json["body_text"].as_str().expect("a reset email");
    let start = body.find("token=").expect("a link in the body") + "token=".len();
    body[start..]
        .split_whitespace()
        .next()
        .expect("a token after the marker")
        .to_owned()
}

async fn request_reset(client: &mut Client, email: &str) -> common::TestResponse {
    client
        .post(
            "/v1/auth/password-reset/request",
            serde_json::json!({ "email": email }),
        )
        .await
}

async fn confirm(client: &mut Client, token: &str, password: &str) -> common::TestResponse {
    client
        .post(
            "/v1/auth/password-reset/confirm",
            serde_json::json!({ "token": token, "new_password": password }),
        )
        .await
}

/// The endpoint must not be an account enumerator: a request for an unknown
/// address is byte-identical to one for a real account.
#[sqlx::test(migrations = "../../migrations")]
async fn requesting_a_reset_for_an_unknown_email_looks_identical_to_a_known_one(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    let mut client = Client::new(router);
    let known = request_reset(&mut client, EMAIL).await;
    let unknown = request_reset(&mut client, "nobody@example.test").await;

    assert_eq!(known.status, StatusCode::NO_CONTENT);
    assert_eq!(unknown.status, known.status);
    assert_eq!(unknown.json, known.json);

    // But only the real account got an email.
    let sent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM email_outbox \
                                         WHERE kind = 'password_reset'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(sent, 1);
}

/// The full loop: request, click, set a new password. The old password stops
/// working, every session dies, and the link is spent.
#[sqlx::test(migrations = "../../migrations")]
async fn a_reset_link_works_exactly_once_and_signs_out_every_session(pool: PgPool) {
    let router = router(pool.clone());
    let mut signed_in = Client::new(router.clone());
    signed_in.register(EMAIL).await;

    let mut resetter = Client::new(router.clone());
    request_reset(&mut resetter, EMAIL).await;
    let token = latest_reset_token(&mut resetter, EMAIL).await;

    let response = confirm(&mut resetter, &token, NEW_PASSWORD).await;
    assert_eq!(
        response.status,
        StatusCode::NO_CONTENT,
        "{:?}",
        response.json
    );

    // The signed-in browser's session is dead.
    let me = signed_in.get("/v1/me").await;
    assert_eq!(
        me.status,
        StatusCode::UNAUTHORIZED,
        "sessions must be revoked"
    );

    // The link is spent.
    let again = confirm(&mut resetter, &token, "yet another long password!!").await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert_eq!(again.json["error"]["code"], "invalid_request");

    // The new password works, the old one does not. (Fresh browser: the code
    // step follows, which is the challenged 202.)
    let mut fresh = Client::new(router);
    let old = fresh
        .post(
            "/v1/auth/login",
            serde_json::json!({ "email": EMAIL, "password": PASSWORD }),
        )
        .await;
    assert_eq!(old.status, StatusCode::UNAUTHORIZED);

    let new = fresh.login(EMAIL, NEW_PASSWORD).await;
    assert_eq!(new.status, StatusCode::OK, "{:?}", new.json);
}

/// A weak replacement is refused without spending the link — every typo must
/// not cost a fresh email.
#[sqlx::test(migrations = "../../migrations")]
async fn a_weak_new_password_leaves_the_link_alive(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    let mut client = Client::new(router);
    request_reset(&mut client, EMAIL).await;
    let token = latest_reset_token(&mut client, EMAIL).await;

    let weak = confirm(&mut client, &token, "short").await;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST);

    let good = confirm(&mut client, &token, NEW_PASSWORD).await;
    assert_eq!(good.status, StatusCode::NO_CONTENT, "{:?}", good.json);
}

/// A fresh request invalidates the previous link: at most one live reset per
/// account.
#[sqlx::test(migrations = "../../migrations")]
async fn a_new_reset_request_invalidates_the_previous_link(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    let mut client = Client::new(router);
    request_reset(&mut client, EMAIL).await;
    let first = latest_reset_token(&mut client, EMAIL).await;
    request_reset(&mut client, EMAIL).await;
    let second = latest_reset_token(&mut client, EMAIL).await;
    assert_ne!(first, second);

    let stale = confirm(&mut client, &first, NEW_PASSWORD).await;
    assert_eq!(stale.status, StatusCode::BAD_REQUEST);

    let live = confirm(&mut client, &second, NEW_PASSWORD).await;
    assert_eq!(live.status, StatusCode::NO_CONTENT, "{:?}", live.json);
}

/// An expired link reads the same as a bogus one.
#[sqlx::test(migrations = "../../migrations")]
async fn an_expired_link_reads_the_same_as_a_bogus_one(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    let mut client = Client::new(router);
    request_reset(&mut client, EMAIL).await;
    let token = latest_reset_token(&mut client, EMAIL).await;

    sqlx::query(
        "UPDATE auth_tokens SET expires_at = now() - interval '1 minute' \
          WHERE purpose = 'password_reset'",
    )
    .execute(&pool)
    .await
    .expect("expire the token");

    let expired = confirm(&mut client, &token, NEW_PASSWORD).await;
    let bogus = confirm(&mut client, "not-a-real-token", NEW_PASSWORD).await;

    assert_eq!(expired.status, StatusCode::BAD_REQUEST);
    assert_eq!(expired.json, bogus.json);
}

/// A completed reset is proof of inbox control, so it verifies the address.
#[sqlx::test(migrations = "../../migrations")]
async fn a_reset_verifies_the_address_too(pool: PgPool) {
    let router = router(pool.clone());

    // Register without completing the code, so the address starts unverified.
    let mut client = Client::new(router);
    client
        .post(
            "/v1/auth/register",
            serde_json::json!({
                "email": EMAIL,
                "display_name": "Test Person",
                "password": PASSWORD,
                "account_type": "homeowner",
            }),
        )
        .await;

    request_reset(&mut client, EMAIL).await;
    let token = latest_reset_token(&mut client, EMAIL).await;
    let response = confirm(&mut client, &token, NEW_PASSWORD).await;
    assert_eq!(
        response.status,
        StatusCode::NO_CONTENT,
        "{:?}",
        response.json
    );

    let verified: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT email_verified_at FROM users")
            .fetch_one(&pool)
            .await
            .expect("user row");
    assert!(verified.is_some(), "the reset proved the inbox");
}

/// Both dimensions of the request limit: per caller address, and per target
/// address across callers.
#[sqlx::test(migrations = "../../migrations")]
async fn reset_requests_are_rate_limited_per_ip_and_per_email(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    // Per IP: 5 in the window, the sixth refused — different targets, so the
    // per-email limit cannot be what refuses it.
    let mut one_caller = Client::new(router.clone()).with_peer("198.51.100.61:5000");
    for n in 0..5 {
        let response = request_reset(&mut one_caller, &format!("target{n}@example.test")).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT, "request {n}");
    }
    let limited = request_reset(&mut one_caller, "target5@example.test").await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);

    // Per target: 3 from distinct addresses, the fourth refused.
    for n in 0..3 {
        let mut caller =
            Client::new(router.clone()).with_peer(&format!("198.51.100.{}:5000", 70 + n));
        let response = request_reset(&mut caller, EMAIL).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT, "caller {n}");
    }
    let mut another = Client::new(router).with_peer("198.51.100.90:5000");
    let flooded = request_reset(&mut another, EMAIL).await;
    assert_eq!(
        flooded.status,
        StatusCode::TOO_MANY_REQUESTS,
        "a victim's inbox must not be floodable from many addresses"
    );
}
