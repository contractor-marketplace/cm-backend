//! The guarantees that only appear under concurrency.
//!
//! Two properties are proved here, and both are invisible to a single-threaded
//! test: that a verification cannot be acted on after the credential it
//! verified has moved, and that hashing never occupies a database connection.

use cm_auth::{AuthService, RequestContext};
use cm_core::{AuthConfig, DatabaseConfig, Origin, Secret};
use cm_db::repo::{passwords, users};
use cm_db::PgPool;
use std::time::{Duration, Instant};

const PASSWORD: &str = "a sufficiently long password";
const REPLACEMENT: &str = "an entirely different long password";
const PEPPER: &str = "test-pepper-that-is-at-least-32-characters";

fn auth_config(argon2_max_concurrency: usize) -> AuthConfig {
    AuthConfig {
        hash_pepper: Secret::new(PEPPER.to_owned()),
        session_idle: Duration::from_secs(14 * 86_400),
        session_absolute: Duration::from_secs(90 * 86_400),
        argon2_max_concurrency,
        trust_proxy_headers: false,
        firebase: None,
    }
}

fn service(argon2_max_concurrency: usize) -> AuthService {
    AuthService::new(
        &auth_config(argon2_max_concurrency),
        Origin::parse("https://app.example.test").expect("origin"),
    )
    .expect("build service")
}

fn context() -> RequestContext {
    RequestContext {
        client_ip: Some("203.0.113.10".to_owned()),
        user_agent: None,
        request_id: None,
    }
}

/// The URL of the throwaway database `#[sqlx::test]` created, so a test can
/// build a pool with its own settings.
fn database_url(pool: &PgPool) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run these tests");
    let database = pool
        .connect_options()
        .get_database()
        .expect("the test pool always names a database")
        .to_owned();

    let (head, query) = match base.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (base.as_str(), None),
    };
    let (authority, _) = head
        .rsplit_once('/')
        .expect("a database URL names a database");

    match query {
        Some(query) => format!("{authority}/{database}?{query}"),
        None => format!("{authority}/{database}"),
    }
}

/// Register and complete the emailed-code step, ending signed in the way a
/// person would — code read from the outbox, which is the test's mailbox.
async fn an_account(service: &AuthService, pool: &PgPool, email: &str) -> uuid::Uuid {
    let challenge = service
        .register(
            pool,
            email,
            "Test Person",
            PASSWORD,
            cm_db::repo::users::AccountType::Homeowner,
            &context(),
        )
        .await
        .expect("register");

    let body: String = sqlx::query_scalar(
        "SELECT body_text FROM email_outbox WHERE recipient = $1 \
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .expect("the challenge email exists");
    let code = body
        .as_bytes()
        .windows(6)
        .position(|window| window.iter().all(u8::is_ascii_digit))
        .map(|start| body[start..start + 6].to_owned())
        .expect("a 6-digit code in the body");

    let (outcome, _device) = service
        .verify_login_code(pool, challenge.challenge_id, &code, &context())
        .await
        .expect("the emailed code signs in");
    outcome.user.id
}

/// A password replaced while a login is in flight must not produce a session.
///
/// Made deterministic by holding the credential's row lock: the login gets
/// through its read and its Argon2 verification, then blocks on the lock. The
/// password is changed and committed while it waits, so when the login finally
/// revalidates, the hash it verified is no longer the account's.
#[sqlx::test(migrations = "../../migrations")]
async fn a_password_replaced_mid_login_cannot_produce_a_session(pool: PgPool) {
    let service = service(2);
    let user_id = an_account(&service, &pool, "race@example.test").await;

    // Take the lock the login will need at its final step.
    let mut blocker = pool.begin().await.expect("begin");
    passwords::find_for_update(&mut blocker, user_id)
        .await
        .expect("lock the credential");

    let login = tokio::spawn({
        let service = service.clone();
        let pool = pool.clone();
        async move {
            service
                .login(&pool, "race@example.test", PASSWORD, None, &context())
                .await
        }
    });

    // Comfortably longer than one Argon2 verification, so the login is parked
    // on the lock rather than still hashing. If it were not, the login would
    // succeed and this test would fail loudly rather than pass by accident.
    tokio::time::sleep(Duration::from_millis(1_000)).await;

    // Replace the password from inside the transaction holding the lock.
    let replacement_hash = a_stored_hash(REPLACEMENT).await;
    let old_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM password_credentials WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&mut *blocker)
            .await
            .expect("read the current hash");
    assert!(
        passwords::replace_hash(&mut blocker, user_id, &old_hash, &replacement_hash)
            .await
            .expect("replace"),
        "the change should win: it holds the lock"
    );
    blocker.commit().await.expect("commit the change");

    let outcome = login.await.expect("join");
    let error = outcome.expect_err("a session must not be issued from the replaced password");
    assert_eq!(
        error.code(),
        "unauthenticated",
        "the obsolete password must read as a failed login"
    );

    let sessions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sessions WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        sessions, 1,
        "only the registration session should exist; the login must not have added one"
    );
}

/// Two changes that both verified the same old password: exactly one succeeds.
///
/// Both are parked on the row lock while they finish hashing, so both reach the
/// final step with the same snapshot. Whichever acquires the lock first swaps
/// the hash; the other's compare-and-swap then finds a value it did not verify.
#[sqlx::test(migrations = "../../migrations")]
async fn two_concurrent_password_changes_produce_exactly_one_success(pool: PgPool) {
    let service = service(4);
    let user_id = an_account(&service, &pool, "double@example.test").await;

    let session_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM sessions WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("session id");

    let mut blocker = pool.begin().await.expect("begin");
    passwords::find_for_update(&mut blocker, user_id)
        .await
        .expect("lock the credential");

    let change = |new_password: &'static str| {
        let service = service.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            service
                .change_password(
                    &pool,
                    user_id,
                    session_id,
                    PASSWORD,
                    new_password,
                    &context(),
                )
                .await
        })
    };
    let first = change(REPLACEMENT);
    let second = change("yet another perfectly long password");

    // Both get through their verification and their new hash, then park.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    blocker.rollback().await.expect("release the lock");

    let results = [first.await.expect("join"), second.await.expect("join")];
    let successes = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(successes, 1, "exactly one change may succeed: {results:?}");

    let loser = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one must fail");
    assert_eq!(
        loser.code(),
        "conflict",
        "the loser must be told the account moved, not that its password was wrong"
    );

    // And the account ended up with exactly one of the two new passwords.
    let changes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'auth.password_changed'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(changes, 1, "the loser must not have written an audit row");
}

/// Queued hashing must not occupy database connections.
///
/// The pool is deliberately tiny and the hasher deliberately serial, so the
/// Argon2 queue lasts far longer than the pool's acquire timeout. If a
/// connection were held across hashing — as it was before this was fixed — the
/// pool would be fully checked out for the whole queue and the concurrent
/// acquisitions below would time out.
#[sqlx::test(migrations = "../../migrations")]
async fn queued_hashing_does_not_occupy_database_connections(pool: PgPool) {
    const LOGINS: usize = 24;
    const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(750);

    let service = service(1);
    an_account(&service, &pool, "storm@example.test").await;

    let constrained = cm_db::connect(&DatabaseConfig {
        url: Secret::new(database_url(&pool)),
        max_connections: 2,
        acquire_timeout: ACQUIRE_TIMEOUT,
    })
    .await
    .expect("build a constrained pool");

    let started = Instant::now();
    let storm: Vec<_> = (0..LOGINS)
        .map(|_| {
            let service = service.clone();
            let pool = constrained.clone();
            tokio::spawn(async move {
                // Wrong password on purpose: the verification still runs, which
                // is the work being queued.
                let _ = service
                    .login(
                        &pool,
                        "storm@example.test",
                        "not the right password",
                        None,
                        &context(),
                    )
                    .await;
            })
        })
        .collect();

    // While the storm runs, ordinary database work must still get a connection.
    let mut acquisitions = 0;
    let mut failures = Vec::new();
    while !storm.iter().all(|task| task.is_finished()) {
        match constrained.acquire().await {
            Ok(conn) => {
                acquisitions += 1;
                drop(conn);
            }
            Err(error) => failures.push(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let elapsed = started.elapsed();

    for task in storm {
        task.await.expect("join");
    }

    assert!(
        failures.is_empty(),
        "the pool was starved by queued hashing: {failures:?}"
    );
    assert!(
        acquisitions > 0,
        "the storm finished before anything was sampled"
    );

    // Guards against the test passing trivially: the queue has to have
    // outlasted the acquire timeout for the assertion above to mean anything.
    assert!(
        elapsed > ACQUIRE_TIMEOUT,
        "the hashing queue ({elapsed:?}) was shorter than the acquire timeout \
         ({ACQUIRE_TIMEOUT:?}), so a held connection would not have been detected"
    );

    constrained.close().await;
}

/// Rate-limit counters must not be lost when the work they guard rolls back.
#[sqlx::test(migrations = "../../migrations")]
async fn a_failed_login_still_counts_against_the_rate_limit(pool: PgPool) {
    let service = service(2);
    an_account(&service, &pool, "counted@example.test").await;

    for _ in 0..3 {
        let _ = service
            .login(
                &pool,
                "counted@example.test",
                "not the right password",
                None,
                &context(),
            )
            .await;
    }

    let counted: i32 =
        sqlx::query_scalar("SELECT max(count) FROM rate_limit_counters WHERE expires_at > now()")
            .fetch_one(&pool)
            .await
            .expect("counter rows");

    assert!(
        counted >= 3,
        "failed logins must be counted even though they return an error: {counted}"
    );
}

/// A real Argon2 hash, for the tests that have to write one directly in order
/// to move the credential out from under a verification.
async fn a_stored_hash(password: &str) -> String {
    let hasher = cm_auth::password::PasswordHasherService::new(1).expect("hasher");
    hasher.hash(password).await.expect("hash")
}

/// Sessions issued by a login must be usable, and a stale credential must not
/// leave a half-written transaction behind.
#[sqlx::test(migrations = "../../migrations")]
async fn a_refused_login_leaves_no_partial_state(pool: PgPool) {
    let service = service(2);
    let user_id = an_account(&service, &pool, "clean@example.test").await;

    // Move the password out from under any future verification.
    let mut conn = pool.acquire().await.expect("connection");
    let old_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM password_credentials WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&mut *conn)
            .await
            .expect("hash");
    let replacement = a_stored_hash(REPLACEMENT).await;
    passwords::replace_hash(&mut conn, user_id, &old_hash, &replacement)
        .await
        .expect("replace");

    let error = service
        .login(&pool, "clean@example.test", PASSWORD, None, &context())
        .await
        .expect_err("the old password must not work");
    assert_eq!(error.code(), "unauthenticated");

    let succeeded: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'auth.login_succeeded'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(
        succeeded, 1,
        "only the registration's code sign-in; the refused login must not add one"
    );

    // The account is otherwise untouched and the new password works: from an
    // unremembered browser, a correct password becomes a code challenge.
    let result = service
        .login(&pool, "clean@example.test", REPLACEMENT, None, &context())
        .await
        .expect("the new password must work");
    assert!(
        matches!(result, cm_auth::LoginResult::Challenged(_)),
        "an unremembered browser gets the code step: {result:?}"
    );

    let mut conn = pool.acquire().await.expect("connection");
    assert_eq!(
        users::find_by_id(&mut conn, user_id)
            .await
            .expect("find")
            .expect("user")
            .status,
        users::UserStatus::Active
    );
}
