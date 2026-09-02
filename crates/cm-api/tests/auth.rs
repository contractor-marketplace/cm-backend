//! Registration, login, lockout, sessions, logout and password change.

mod common;

use common::{router, Client, PASSWORD};
use http::StatusCode;
use sqlx::PgPool;
use sqlx::Row;

const EMAIL: &str = "marisol@example.test";

// ── registration ────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../../migrations")]
async fn registration_creates_an_account_and_signs_it_in(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));

    // `register` completes the emailed-code step, so the response here is the
    // final one: a session, and an address the code round-trip just verified.
    let response = client.register(EMAIL).await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["user"]["email"], EMAIL);
    assert_eq!(response.json["user"]["status"], "active");
    assert_eq!(response.json["user"]["email_verified"], true);
    assert!(response.json["csrf_token"].is_string());

    // The session works straight away, without a separate login.
    let me = client.get("/v1/me").await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json["user"]["email"], EMAIL);
    assert_eq!(me.json["roles"].as_array().expect("roles").len(), 0);

    let stored: String = sqlx::query_scalar("SELECT password_hash FROM password_credentials")
        .fetch_one(&pool)
        .await
        .expect("credential row");
    assert!(stored.starts_with("$argon2id$"), "{stored}");
    assert!(
        !stored.contains(PASSWORD),
        "the password must not be stored"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_session_cookie_carries_every_required_attribute(pool: PgPool) {
    let mut client = Client::new(router(pool));
    let response = client.register(EMAIL).await;

    let session = response
        .cookie("__Host-cm_session")
        .expect("a session cookie");
    assert!(session.contains("; Secure"), "{session}");
    assert!(session.contains("; HttpOnly"), "{session}");
    assert!(session.contains("; Path=/"), "{session}");
    assert!(session.contains("; SameSite=Lax"), "{session}");
    assert!(
        !session.to_lowercase().contains("domain="),
        "a __Host- cookie with a Domain attribute is rejected by browsers: {session}"
    );

    let csrf = response.cookie("__Host-cm_csrf").expect("a csrf cookie");
    assert!(csrf.contains("; Secure"), "{csrf}");
    assert!(
        !csrf.contains("HttpOnly"),
        "the front end has to read the CSRF cookie: {csrf}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_raw_session_token_is_never_stored(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    client.register(EMAIL).await;

    let token = client.session_cookie().expect("a session token").to_owned();
    let rows = sqlx::query("SELECT token_hash FROM sessions")
        .fetch_all(&pool)
        .await
        .expect("session rows");

    assert_eq!(rows.len(), 1);
    let hash: Vec<u8> = rows[0].get("token_hash");
    assert_eq!(hash.len(), 32, "a SHA-256 digest is 32 bytes");
    assert_ne!(hash, token.as_bytes(), "the raw token must not be stored");
    assert!(
        !String::from_utf8_lossy(&hash).contains(&token),
        "the digest must not contain the token"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_duplicate_address_is_refused(pool: PgPool) {
    let router = router(pool);
    assert_eq!(
        Client::new(router.clone()).register(EMAIL).await.status,
        StatusCode::OK
    );

    // A different case of the same address is the same address: the unique
    // index is on the generated `email_norm`, not on what was typed.
    let again = Client::new(router).register("MARISOL@Example.TEST").await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.json["error"]["code"], "conflict");
}

#[sqlx::test(migrations = "../../migrations")]
async fn registration_rejects_weak_or_malformed_input(pool: PgPool) {
    let router = router(pool);

    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "short password",
            serde_json::json!({"email": EMAIL, "display_name": "A", "password": "short"}),
        ),
        (
            "denylisted password",
            serde_json::json!({"email": EMAIL, "display_name": "A", "password": "passwordpassword"}),
        ),
        (
            "password contains the address",
            serde_json::json!({"email": EMAIL, "display_name": "A", "password": "marisol-marisol-1"}),
        ),
        (
            "not an address",
            serde_json::json!({"email": "not-an-address", "display_name": "A", "password": PASSWORD}),
        ),
        (
            "blank display name",
            serde_json::json!({"email": EMAIL, "display_name": "   ", "password": PASSWORD}),
        ),
    ];

    for (label, body) in cases {
        let mut client = Client::new(router.clone());
        let response = client.post("/v1/auth/register", body).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{label} should be rejected, got {:?}",
            response.json
        );
        assert_eq!(response.json["error"]["code"], "invalid_request", "{label}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_malformed_body_uses_the_shared_error_envelope(pool: PgPool) {
    let mut client = Client::new(router(pool));

    let response = client
        .post("/v1/auth/login", serde_json::json!({ "email": EMAIL }))
        .await;

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.json["error"]["code"], "invalid_request");
    assert!(response.json["error"]["message"].is_string());
}

// ── login ───────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../../migrations")]
async fn login_issues_a_new_session_distinct_from_the_first(pool: PgPool) {
    let router = router(pool);

    let mut first = Client::new(router.clone());
    first.register(EMAIL).await;
    let registration_token = first.session_cookie().expect("token").to_owned();

    let mut second = Client::new(router);
    let response = second.login(EMAIL, PASSWORD).await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);

    let login_token = second.session_cookie().expect("token");
    assert_ne!(
        registration_token, login_token,
        "each login must mint its own token"
    );

    // Both sessions are live: logging in elsewhere does not sign you out here.
    assert_eq!(first.get("/v1/me").await.status, StatusCode::OK);
    assert_eq!(second.get("/v1/me").await.status, StatusCode::OK);
}

/// The response to a wrong password and to an address with no account must be
/// identical, or the endpoint is an account enumerator.
#[sqlx::test(migrations = "../../migrations")]
async fn every_login_failure_looks_the_same(pool: PgPool) {
    let router = router(pool);
    Client::new(router.clone()).register(EMAIL).await;

    let wrong_password = Client::new(router.clone())
        .login(EMAIL, "an entirely different password")
        .await;
    let unknown_account = Client::new(router)
        .login("nobody@example.test", PASSWORD)
        .await;

    assert_eq!(wrong_password.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown_account.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong_password.json, unknown_account.json,
        "the two failures must be byte-identical"
    );
    assert!(
        wrong_password.set_cookies().is_empty(),
        "no cookie on failure"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_suspended_account_cannot_log_in_or_use_an_existing_session(pool: PgPool) {
    let router = router(pool.clone());
    let mut client = Client::new(router.clone());
    client.register(EMAIL).await;
    assert_eq!(client.get("/v1/me").await.status, StatusCode::OK);

    sqlx::query("UPDATE users SET status = 'suspended'")
        .execute(&pool)
        .await
        .expect("suspend");

    // The live session stops working immediately, without having to hunt it
    // down and revoke it.
    assert_eq!(client.get("/v1/me").await.status, StatusCode::UNAUTHORIZED);

    let fresh = Client::new(router).login(EMAIL, PASSWORD).await;
    assert_eq!(fresh.status, StatusCode::UNAUTHORIZED);
}

// ── lockout ─────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../../migrations")]
async fn eight_failures_lock_the_account_against_the_correct_password(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    for attempt in 1..=8 {
        let response = Client::new(router.clone())
            .login(EMAIL, "wrong password here")
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt}"
        );
    }

    let (attempts, locked): (i32, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT failed_attempts, locked_until FROM password_credentials")
            .fetch_one(&pool)
            .await
            .expect("credential row");
    assert_eq!(attempts, 8);
    assert!(locked.is_some(), "the account should be locked");

    // The decisive assertion: the *correct* password is now refused.
    let correct = Client::new(router).login(EMAIL, PASSWORD).await;
    assert_eq!(correct.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_lockout_applies_to_one_account_only(pool: PgPool) {
    let router = router(pool);
    Client::new(router.clone()).register(EMAIL).await;
    Client::new(router.clone())
        .register("other@example.test")
        .await;

    for _ in 0..8 {
        Client::new(router.clone())
            .login(EMAIL, "wrong password here")
            .await;
    }

    let other = Client::new(router)
        .login("other@example.test", PASSWORD)
        .await;
    assert_eq!(
        other.status,
        StatusCode::OK,
        "one account's lockout must not affect another"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_successful_login_clears_the_failure_counter(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    for _ in 0..3 {
        Client::new(router.clone())
            .login(EMAIL, "wrong password here")
            .await;
    }
    let before: i32 = sqlx::query_scalar("SELECT failed_attempts FROM password_credentials")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(before, 3);

    assert_eq!(
        Client::new(router).login(EMAIL, PASSWORD).await.status,
        StatusCode::OK
    );

    let after: i32 = sqlx::query_scalar("SELECT failed_attempts FROM password_credentials")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(after, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_expired_lock_lets_the_account_back_in(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    for _ in 0..8 {
        Client::new(router.clone())
            .login(EMAIL, "wrong password here")
            .await;
    }
    sqlx::query("UPDATE password_credentials SET locked_until = now() - interval '1 minute'")
        .execute(&pool)
        .await
        .expect("expire the lock");

    let response = Client::new(router).login(EMAIL, PASSWORD).await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);

    let (attempts, locked): (i32, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT failed_attempts, locked_until FROM password_credentials")
            .fetch_one(&pool)
            .await
            .expect("credential row");
    assert_eq!(attempts, 0);
    assert!(locked.is_none());
}

// ── sessions ────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../../migrations")]
async fn a_revoked_or_expired_session_stops_working(pool: PgPool) {
    let router = router(pool.clone());

    let mut revoked = Client::new(router.clone());
    revoked.register(EMAIL).await;
    sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'admin'")
        .execute(&pool)
        .await
        .expect("revoke");
    assert_eq!(revoked.get("/v1/me").await.status, StatusCode::UNAUTHORIZED);

    let mut expired = Client::new(router.clone());
    expired.register("second@example.test").await;
    sqlx::query(
        "UPDATE sessions SET idle_expires_at = now() - interval '1 second' \
         WHERE revoked_at IS NULL",
    )
    .execute(&pool)
    .await
    .expect("expire");
    assert_eq!(expired.get("/v1/me").await.status, StatusCode::UNAUTHORIZED);

    // Absolute expiry. The schema forbids an idle window outliving the absolute
    // one, so an absolutely-expired session is necessarily idle-expired too;
    // the row has to be aged rather than simply back-dated.
    let mut absolute = Client::new(router);
    absolute.register("third@example.test").await;
    sqlx::query(
        "UPDATE sessions \
            SET created_at = now() - interval '100 days', \
                absolute_expires_at = now() - interval '1 second', \
                idle_expires_at = now() - interval '1 second' \
          WHERE revoked_at IS NULL",
    )
    .execute(&pool)
    .await
    .expect("age the session");
    assert_eq!(
        absolute.get("/v1/me").await.status,
        StatusCode::UNAUTHORIZED
    );
}

/// The schema, not the application, is what guarantees a session cannot outlive
/// its absolute ceiling by having a longer idle window.
#[sqlx::test(migrations = "../../migrations")]
async fn the_schema_refuses_an_idle_window_beyond_the_absolute_one(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    client.register(EMAIL).await;

    let result =
        sqlx::query("UPDATE sessions SET idle_expires_at = absolute_expires_at + interval '1 day'")
            .execute(&pool)
            .await;

    assert!(
        result.is_err(),
        "an idle window past the absolute ceiling must be rejected by the database"
    );
}

// ── logout ──────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../../migrations")]
async fn logout_revokes_the_session_and_clears_the_cookies(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    client.register(EMAIL).await;
    let token = client.session_cookie().expect("token").to_owned();

    let response = client.post("/v1/auth/logout", serde_json::json!({})).await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let cleared = response.set_cookies();
    assert!(
        cleared.iter().any(|c| c.starts_with("__Host-cm_session=;")),
        "{cleared:?}"
    );
    assert!(
        cleared.iter().any(|c| c.starts_with("__Host-cm_csrf=;")),
        "{cleared:?}"
    );

    // The token is dead even for a client that kept it.
    let mut replay = Client::new(router(pool.clone()));
    replay.set_session(&token);
    assert_eq!(replay.get("/v1/me").await.status, StatusCode::UNAUTHORIZED);

    let reason: Option<String> = sqlx::query_scalar("SELECT revoked_reason FROM sessions")
        .fetch_one(&pool)
        .await
        .expect("row");
    assert_eq!(reason.as_deref(), Some("logout"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn logout_all_ends_every_session_for_that_account_only(pool: PgPool) {
    let router = router(pool);

    let mut one = Client::new(router.clone());
    one.register(EMAIL).await;
    let mut two = Client::new(router.clone());
    two.login(EMAIL, PASSWORD).await;
    let mut bystander = Client::new(router);
    bystander.register("other@example.test").await;

    let response = one.post("/v1/auth/logout-all", serde_json::json!({})).await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    assert_eq!(one.get("/v1/me").await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(two.get("/v1/me").await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        bystander.get("/v1/me").await.status,
        StatusCode::OK,
        "another account's sessions must be untouched"
    );
}

// ── password change ─────────────────────────────────────────────────────────

const NEW_PASSWORD: &str = "an even longer replacement password";

#[sqlx::test(migrations = "../../migrations")]
async fn changing_a_password_rotates_this_session_and_revokes_the_others(pool: PgPool) {
    let router = router(pool.clone());

    let mut here = Client::new(router.clone());
    here.register(EMAIL).await;
    let old_token = here.session_cookie().expect("token").to_owned();

    let mut elsewhere = Client::new(router.clone());
    elsewhere.login(EMAIL, PASSWORD).await;

    let response = here
        .post(
            "/v1/auth/password",
            serde_json::json!({ "current_password": PASSWORD, "new_password": NEW_PASSWORD }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);

    // Rotated: a new token, and the old one is dead.
    let new_token = here.session_cookie().expect("token");
    assert_ne!(old_token, new_token, "the session must be rotated");
    assert_eq!(here.get("/v1/me").await.status, StatusCode::OK);

    let mut replay = Client::new(router.clone());
    replay.set_session(&old_token);
    assert_eq!(replay.get("/v1/me").await.status, StatusCode::UNAUTHORIZED);

    // Every other session is gone.
    assert_eq!(
        elsewhere.get("/v1/me").await.status,
        StatusCode::UNAUTHORIZED
    );

    // The old password no longer works, the new one does.
    assert_eq!(
        Client::new(router.clone())
            .login(EMAIL, PASSWORD)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        Client::new(router).login(EMAIL, NEW_PASSWORD).await.status,
        StatusCode::OK
    );

    let reasons: Vec<String> = sqlx::query_scalar(
        "SELECT revoked_reason FROM sessions WHERE revoked_reason IS NOT NULL ORDER BY revoked_reason",
    )
    .fetch_all(&pool)
    .await
    .expect("reasons");
    assert_eq!(reasons, vec!["password_change", "rotation"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_password_change_needs_the_current_password_and_a_different_new_one(pool: PgPool) {
    let mut client = Client::new(router(pool));
    client.register(EMAIL).await;

    let wrong = client
        .post(
            "/v1/auth/password",
            serde_json::json!({ "current_password": "not it at all", "new_password": NEW_PASSWORD }),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);

    let same = client
        .post(
            "/v1/auth/password",
            serde_json::json!({ "current_password": PASSWORD, "new_password": PASSWORD }),
        )
        .await;
    assert_eq!(same.status, StatusCode::BAD_REQUEST);

    let weak = client
        .post(
            "/v1/auth/password",
            serde_json::json!({ "current_password": PASSWORD, "new_password": "short" }),
        )
        .await;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST);

    // A rejected change leaves the original password working.
    assert_eq!(client.get("/v1/me").await.status, StatusCode::OK);
}

// ── roles ───────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../../migrations")]
async fn roles_are_reported_once_granted(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    client.register(EMAIL).await;

    assert_eq!(
        client.get("/v1/me").await.json["roles"]
            .as_array()
            .expect("roles")
            .len(),
        0,
        "registration grants no roles"
    );

    let user_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(&pool)
        .await
        .expect("user id");
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::grant_role(&mut conn, user_id, cm_db::repo::users::Role::Admin, None)
        .await
        .expect("grant");

    let me = client.get("/v1/me").await;
    assert_eq!(me.json["roles"], serde_json::json!(["admin"]));
}
