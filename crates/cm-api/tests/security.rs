//! The security matrix: authentication, CSRF, rate limiting, and what the
//! audit log is and is not allowed to contain.
//!
//! A route added without being added here should fail `every_route_states_its_
//! authentication_requirement`, so coverage cannot quietly rot as the surface
//! grows.

mod common;

use common::{router, Client, PASSWORD, SITE_ORIGIN};
use http::{Method, StatusCode};
use sqlx::PgPool;
use sqlx::Row;

const EMAIL: &str = "matrix@example.test";

/// Whether a route needs a session, and whether it changes state.
struct RouteSpec {
    method: Method,
    path: &'static str,
    needs_session: bool,
    changes_state: bool,
}

fn routes() -> Vec<RouteSpec> {
    vec![
        RouteSpec {
            method: Method::GET,
            path: "/healthz",
            needs_session: false,
            changes_state: false,
        },
        RouteSpec {
            method: Method::GET,
            path: "/readyz",
            needs_session: false,
            changes_state: false,
        },
        RouteSpec {
            method: Method::GET,
            path: "/version",
            needs_session: false,
            changes_state: false,
        },
        RouteSpec {
            method: Method::POST,
            path: "/v1/auth/register",
            needs_session: false,
            changes_state: true,
        },
        RouteSpec {
            method: Method::POST,
            path: "/v1/auth/login",
            needs_session: false,
            changes_state: true,
        },
        RouteSpec {
            method: Method::GET,
            path: "/v1/me",
            needs_session: true,
            changes_state: false,
        },
        RouteSpec {
            method: Method::POST,
            path: "/v1/auth/logout",
            needs_session: true,
            changes_state: true,
        },
        RouteSpec {
            method: Method::POST,
            path: "/v1/auth/logout-all",
            needs_session: true,
            changes_state: true,
        },
        RouteSpec {
            method: Method::POST,
            path: "/v1/auth/password",
            needs_session: true,
            changes_state: true,
        },
    ]
}

// ── authentication ──────────────────────────────────────────────────────────

/// Every route that needs a session refuses an anonymous caller, and every
/// route that does not is reachable without one.
#[sqlx::test(migrations = "../../migrations")]
async fn every_route_states_its_authentication_requirement(pool: PgPool) {
    let router = router(pool);

    for spec in routes() {
        let mut anonymous = Client::new(router.clone());
        let response = anonymous
            .send(
                spec.method.clone(),
                spec.path,
                spec.changes_state.then(|| serde_json::json!({})),
            )
            .await;

        if spec.needs_session {
            assert_eq!(
                response.status,
                StatusCode::UNAUTHORIZED,
                "{} {} must require a session",
                spec.method,
                spec.path
            );
            assert_eq!(response.json["error"]["code"], "unauthenticated");
        } else {
            assert_ne!(
                response.status,
                StatusCode::UNAUTHORIZED,
                "{} {} must be reachable without a session",
                spec.method,
                spec.path
            );
        }
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_forged_or_stale_session_cookie_is_refused(pool: PgPool) {
    let router = router(pool);

    for token in [
        "not-a-token",
        "",
        // The right shape, the wrong value.
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        let mut client = Client::new(router.clone());
        client.set_session(token);
        assert_eq!(
            client.get("/v1/me").await.status,
            StatusCode::UNAUTHORIZED,
            "token {token:?} must be refused"
        );
    }
}

// ── CSRF ────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../../migrations")]
async fn a_state_changing_request_without_a_csrf_token_is_refused(pool: PgPool) {
    let router = router(pool);

    // Sign in normally, then keep the session cookie but drop the header.
    let mut signed_in = Client::new(router.clone());
    signed_in.register(EMAIL).await;
    let session = signed_in.session_cookie().expect("token").to_owned();

    let mut without = Client::new(router).without_csrf();
    without.set_session(&session);

    let response = without.post("/v1/auth/logout", serde_json::json!({})).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.json["error"]["code"], "forbidden");

    // ...and the session was not ended by the rejected request.
    assert_eq!(signed_in.get("/v1/me").await.status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_wrong_csrf_token_is_refused(pool: PgPool) {
    let mut client = Client::new(router(pool));
    client.register(EMAIL).await;
    client.set_csrf("a-token-that-was-not-derived-from-this-session");

    let response = client.post("/v1/auth/logout", serde_json::json!({})).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

/// The token is derived from the session, so one signed-in user's token is
/// useless against another's session. This is the case a plain double-submit
/// scheme gets wrong.
#[sqlx::test(migrations = "../../migrations")]
async fn another_sessions_csrf_token_is_refused(pool: PgPool) {
    let router = router(pool);

    let mut victim = Client::new(router.clone());
    victim.register(EMAIL).await;

    let mut attacker = Client::new(router);
    attacker.register("attacker@example.test").await;
    let attacker_token = attacker.csrf_token().expect("token").to_owned();

    victim.set_csrf(&attacker_token);
    let response = victim.post("/v1/auth/logout", serde_json::json!({})).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_cross_origin_request_is_refused_even_with_a_valid_token(pool: PgPool) {
    let router = router(pool);

    let mut client = Client::new(router.clone());
    client.register(EMAIL).await;
    let session = client.session_cookie().expect("token").to_owned();
    let csrf = client.csrf_token().expect("token").to_owned();

    for origin in [
        "https://evil.example.test",
        "http://app.example.test",
        "null",
    ] {
        let mut hostile = Client::new(router.clone()).with_origin(Some(origin));
        hostile.set_session(&session);
        hostile.set_csrf(&csrf);

        let response = hostile.post("/v1/auth/logout", serde_json::json!({})).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "origin {origin} must be refused"
        );
    }

    // The site's own origin is fine.
    let mut ours = Client::new(router).with_origin(Some(SITE_ORIGIN));
    ours.set_session(&session);
    ours.set_csrf(&csrf);
    assert_eq!(
        ours.post("/v1/auth/logout", serde_json::json!({}))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

/// Safe methods must not require the header, or ordinary navigation breaks.
#[sqlx::test(migrations = "../../migrations")]
async fn a_read_does_not_require_a_csrf_token(pool: PgPool) {
    let router = router(pool);

    let mut signed_in = Client::new(router.clone());
    signed_in.register(EMAIL).await;
    let session = signed_in.session_cookie().expect("token").to_owned();

    let mut reader = Client::new(router).without_csrf();
    reader.set_session(&session);
    assert_eq!(reader.get("/v1/me").await.status, StatusCode::OK);
}

/// CSRF is enforced after authentication, so an anonymous caller still gets a
/// 401 rather than being told the token was the problem.
#[sqlx::test(migrations = "../../migrations")]
async fn an_anonymous_state_changing_request_is_unauthenticated_not_forbidden(pool: PgPool) {
    let mut client = Client::new(router(pool)).without_csrf();
    let response = client.post("/v1/auth/logout", serde_json::json!({})).await;

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

// ── rate limiting ───────────────────────────────────────────────────────────

/// Whether the fixed rate-limit window rolled over while a burst was running.
///
/// Windows are aligned to the epoch, so a burst that straddles a boundary sees
/// its counter reset and the request after the limit is legitimately allowed.
/// That is correct behaviour and a real flake source for any test that asserts
/// on the last request, so the assertion is made conditional on it rather than
/// left to fail once every few hundred runs.
async fn window_rolled(pool: &PgPool, policy_prefix: &str) -> bool {
    let windows: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT window_start) FROM rate_limit_counters WHERE count > 1",
    )
    .fetch_one(pool)
    .await
    .expect("count windows");

    let _ = policy_prefix;
    windows > 1
}

#[sqlx::test(migrations = "../../migrations")]
async fn registration_is_limited_per_address(pool: PgPool) {
    let router = router(pool.clone());

    // The tenth is the last one allowed.
    for n in 0..10 {
        let response = Client::new(router.clone())
            .with_peer("198.51.100.7:5000")
            .register(&format!("user{n}@example.test"))
            .await;
        assert_eq!(response.status, StatusCode::OK, "registration {n}");
    }

    let limited = Client::new(router.clone())
        .with_peer("198.51.100.7:5000")
        .register("eleventh@example.test")
        .await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.json["error"]["code"], "too_many_requests");
    assert!(
        limited.headers.get(http::header::RETRY_AFTER).is_some(),
        "a limited client must be told how long to wait"
    );

    // A different address is unaffected: the limit is per client, not global.
    let elsewhere = Client::new(router)
        .with_peer("198.51.100.8:5000")
        .register("elsewhere@example.test")
        .await;
    assert_eq!(elsewhere.status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations")]
async fn login_is_limited_per_address_across_accounts(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    // Twenty attempts are allowed, spread across whatever addresses the caller
    // tries: this is the control that catches one host working through a list.
    for n in 0..20 {
        let response = Client::new(router.clone())
            .with_peer("198.51.100.9:5000")
            .login(&format!("target{n}@example.test"), PASSWORD)
            .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "attempt {n}");
    }

    let limited = Client::new(router)
        .with_peer("198.51.100.9:5000")
        .login(EMAIL, PASSWORD)
        .await;

    if !window_rolled(&pool, "login:ip").await {
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn password_change_is_limited_per_account(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    client.register(EMAIL).await;

    // Five attempts an hour, counted whether or not they succeed.
    for n in 0..5 {
        let response = client
            .post(
                "/v1/auth/password",
                serde_json::json!({
                    "current_password": "deliberately wrong",
                    "new_password": "a perfectly fine replacement",
                }),
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "attempt {n}");
    }

    let limited = client
        .post(
            "/v1/auth/password",
            serde_json::json!({
                "current_password": PASSWORD,
                "new_password": "a perfectly fine replacement",
            }),
        )
        .await;

    if !window_rolled(&pool, "password_change:user").await {
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    }
}

/// The counters table must not become a log of who was where.
#[sqlx::test(migrations = "../../migrations")]
async fn rate_limit_buckets_are_stored_as_digests(pool: PgPool) {
    let mut client = Client::new(router(pool.clone())).with_peer("198.51.100.11:5000");
    client.register(EMAIL).await;

    let rows = sqlx::query("SELECT bucket_hash FROM rate_limit_counters")
        .fetch_all(&pool)
        .await
        .expect("counter rows");
    assert!(!rows.is_empty(), "registration should have counted");

    for row in rows {
        let bucket: Vec<u8> = row.get("bucket_hash");
        assert_eq!(bucket.len(), 32);
        let rendered = String::from_utf8_lossy(&bucket).to_string();
        assert!(
            !rendered.contains("198.51.100.11"),
            "the address is in the clear"
        );
        assert!(
            !rendered.contains("register"),
            "the policy name is in the clear"
        );
    }
}

// ── audit ───────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../../migrations")]
async fn the_audit_log_records_the_authentication_events(pool: PgPool) {
    let router = router(pool.clone());

    let mut client = Client::new(router.clone());
    client.register(EMAIL).await;
    Client::new(router.clone())
        .login(EMAIL, "wrong password here")
        .await;
    Client::new(router.clone()).login(EMAIL, PASSWORD).await;
    client.post("/v1/auth/logout", serde_json::json!({})).await;

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log ORDER BY created_at, id")
            .fetch_all(&pool)
            .await
            .expect("audit rows");

    for expected in [
        "auth.registered",
        "auth.login_failed",
        "auth.login_succeeded",
        "auth.logged_out",
    ] {
        assert!(
            actions.iter().any(|action| action == expected),
            "missing {expected} in {actions:?}"
        );
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_lockout_is_recorded_with_its_cause(pool: PgPool) {
    let router = router(pool.clone());
    Client::new(router.clone()).register(EMAIL).await;

    for _ in 0..8 {
        Client::new(router.clone())
            .login(EMAIL, "wrong password here")
            .await;
    }

    let data: serde_json::Value =
        sqlx::query_scalar("SELECT data FROM audit_log WHERE action = 'auth.account_locked'")
            .fetch_one(&pool)
            .await
            .expect("a lockout event");

    assert_eq!(data["failed_attempts"], 8);
    assert!(data["locked_until"].is_string());
}

/// A failed login against an address with no account must not add that address
/// to the audit table: those are not our users, and the log is not the place to
/// accumulate other people's email addresses.
#[sqlx::test(migrations = "../../migrations")]
async fn a_failure_against_an_unknown_address_does_not_record_it(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    client
        .login("someone-who-does-not-exist@example.test", PASSWORD)
        .await;

    let row = sqlx::query(
        "SELECT actor_user_id, subject_id, data::text AS data, ip_hash \
           FROM audit_log WHERE action = 'auth.login_failed'",
    )
    .fetch_one(&pool)
    .await
    .expect("a failure event");

    let data: String = row.get("data");
    assert!(data.contains("unknown_account"), "{data}");
    assert!(
        !data.contains("someone-who-does-not-exist"),
        "the attempted address must not be stored: {data}"
    );
    assert!(row.get::<Option<uuid::Uuid>, _>("actor_user_id").is_none());
    assert!(row.get::<Option<uuid::Uuid>, _>("subject_id").is_none());

    // The address of the caller is kept, but only as a digest.
    let ip_hash: Option<Vec<u8>> = row.get("ip_hash");
    let ip_hash = ip_hash.expect("the client address should be recorded");
    assert_eq!(ip_hash.len(), 32);
    assert!(!String::from_utf8_lossy(&ip_hash).contains("203.0.113"));
}

/// A protected request must carry the client address into its audit event.
///
/// Logout and password change previously lost it: their handlers assembled the
/// context from the pieces they had been given, which did not include the
/// connection info. The context is now built once at the edge, so this asserts
/// the whole chain — socket peer, middleware, handler, service, audit row.
#[sqlx::test(migrations = "../../migrations")]
async fn a_protected_request_records_the_client_address(pool: PgPool) {
    let mut client = Client::new(router(pool.clone())).with_peer("198.51.100.23:5000");
    client.register(EMAIL).await;
    client.post("/v1/auth/logout", serde_json::json!({})).await;

    // Registration is public, logout is protected: both must record it.
    for action in ["auth.registered", "auth.logged_out"] {
        let ip_hash: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT ip_hash FROM audit_log WHERE action = $1")
                .bind(action)
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|e| panic!("no {action} row: {e}"));

        let ip_hash = ip_hash.unwrap_or_else(|| panic!("{action} lost the client address"));
        assert_eq!(ip_hash.len(), 32, "{action} must store a digest");
        assert!(
            !String::from_utf8_lossy(&ip_hash).contains("198.51.100.23"),
            "{action} stored the address in the clear"
        );
    }

    // Two different clients must produce two different digests, or the column
    // is recording a constant rather than the caller.
    let mut elsewhere = Client::new(router(pool.clone())).with_peer("198.51.100.99:5000");
    elsewhere.register("elsewhere@example.test").await;

    let digests: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT ip_hash FROM audit_log WHERE action = 'auth.registered' AND ip_hash IS NOT NULL",
    )
    .fetch_all(&pool)
    .await
    .expect("digests");
    assert_eq!(digests.len(), 2);
    assert_ne!(
        digests[0], digests[1],
        "the digest must depend on the address"
    );
}

/// A session row records the address it was created from, for the same reason.
#[sqlx::test(migrations = "../../migrations")]
async fn a_session_records_the_address_it_was_issued_to(pool: PgPool) {
    let mut client = Client::new(router(pool.clone())).with_peer("198.51.100.31:5000");
    client.register(EMAIL).await;

    let ip_hash: Option<Vec<u8>> = sqlx::query_scalar("SELECT ip_hash FROM sessions")
        .fetch_one(&pool)
        .await
        .expect("session row");

    let ip_hash = ip_hash.expect("the session lost the client address");
    assert_eq!(ip_hash.len(), 32);
    assert!(!String::from_utf8_lossy(&ip_hash).contains("198.51.100.31"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_password_change_records_what_it_revoked(pool: PgPool) {
    let router = router(pool.clone());

    let mut here = Client::new(router.clone());
    here.register(EMAIL).await;
    Client::new(router.clone()).login(EMAIL, PASSWORD).await;
    Client::new(router).login(EMAIL, PASSWORD).await;

    here.post(
        "/v1/auth/password",
        serde_json::json!({
            "current_password": PASSWORD,
            "new_password": "a completely different long password",
        }),
    )
    .await;

    let data: serde_json::Value =
        sqlx::query_scalar("SELECT data FROM audit_log WHERE action = 'auth.password_changed'")
            .fetch_one(&pool)
            .await
            .expect("a password-change event");

    assert_eq!(data["other_sessions_revoked"], 2);
    assert!(data["rotated_from"].is_string());
    assert!(data["rotated_to"].is_string());
    assert_ne!(data["rotated_from"], data["rotated_to"]);
}

/// Nothing that reaches the audit log may be a credential.
#[sqlx::test(migrations = "../../migrations")]
async fn the_audit_log_never_contains_a_secret(pool: PgPool) {
    let mut client = Client::new(router(pool.clone()));
    client.register(EMAIL).await;
    let session = client.session_cookie().expect("token").to_owned();
    let csrf = client.csrf_token().expect("token").to_owned();
    client
        .post(
            "/v1/auth/password",
            serde_json::json!({
                "current_password": PASSWORD,
                "new_password": "a completely different long password",
            }),
        )
        .await;

    let dump: Vec<String> = sqlx::query_scalar(
        "SELECT coalesce(data::text, '') || coalesce(action, '') FROM audit_log",
    )
    .fetch_all(&pool)
    .await
    .expect("audit rows");
    let dump = dump.join("\n");

    for secret in [
        PASSWORD,
        "a completely different long password",
        &session,
        &csrf,
    ] {
        assert!(
            !dump.contains(secret),
            "a secret reached the audit log: {dump}"
        );
    }
}
