//! Repository-level guarantees, including the ones that only appear under
//! concurrency.

use chrono::{Duration as ChronoDuration, Utc};
use cm_core::new_id;
use cm_db::repo::rate_limit;
use cm_db::repo::sessions::{self, RevocationReason};
use cm_db::repo::users::{self, Role};
use cm_db::repo::{audit, passwords};
use sqlx::PgPool;

async fn a_user(pool: &PgPool, email: &str) -> uuid::Uuid {
    let mut conn = pool.acquire().await.expect("connection");
    users::insert(&mut conn, new_id(), email, "Test Person")
        .await
        .expect("insert user")
        .id
}

/// Two simultaneous registrations of the same address: exactly one wins, and
/// the loser gets a conflict rather than a 500.
#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_registration_of_one_address_produces_one_account(pool: PgPool) {
    let attempt = |pool: PgPool| async move {
        let mut conn = pool.acquire().await.expect("connection");
        users::insert(&mut conn, new_id(), "race@example.test", "Racer").await
    };

    let (first, second) = tokio::join!(attempt(pool.clone()), attempt(pool.clone()));

    let winners = [first.is_ok(), second.is_ok()]
        .iter()
        .filter(|ok| **ok)
        .count();
    assert_eq!(winners, 1, "exactly one insert must succeed");

    let loser = if first.is_err() { first } else { second };
    let error = loser.expect_err("one must fail");
    assert_eq!(
        error.code(),
        "conflict",
        "a duplicate address is a conflict, not an internal error"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 1);
}

/// Concurrent failed logins must not lose increments: eight parallel failures
/// have to land the account exactly on the lockout threshold.
#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_failures_are_counted_exactly(pool: PgPool) {
    let user_id = a_user(&pool, "counter@example.test").await;
    let mut conn = pool.acquire().await.expect("connection");
    passwords::insert(&mut conn, user_id, "$argon2id$placeholder")
        .await
        .expect("insert credential");
    drop(conn);

    let mut tasks = Vec::new();
    for _ in 0..passwords::MAX_FAILED_ATTEMPTS {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("connection");
            passwords::record_failure(&mut conn, user_id)
                .await
                .expect("record");
        }));
    }
    for task in tasks {
        task.await.expect("join");
    }

    let mut conn = pool.acquire().await.expect("connection");
    let credential = passwords::find(&mut conn, user_id)
        .await
        .expect("find")
        .expect("credential");

    assert_eq!(
        credential.failed_attempts,
        passwords::MAX_FAILED_ATTEMPTS,
        "no increment may be lost"
    );
    assert!(
        credential.is_locked_at(Utc::now()),
        "the threshold must lock the account"
    );
}

/// The same guarantee for rate-limit counters.
#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_rate_limit_hits_are_counted_exactly(pool: PgPool) {
    let bucket = vec![7u8; 32];
    let window = ChronoDuration::minutes(15);
    let now = Utc::now();

    let mut tasks = Vec::new();
    for _ in 0..25 {
        let pool = pool.clone();
        let bucket = bucket.clone();
        tasks.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("connection");
            rate_limit::hit(&mut conn, &bucket, 20, window, now)
                .await
                .expect("hit")
                .count
        }));
    }

    let mut counts = Vec::new();
    for task in tasks {
        counts.push(task.await.expect("join"));
    }
    counts.sort_unstable();

    assert_eq!(
        counts,
        (1..=25).collect::<Vec<i32>>(),
        "every caller must see a distinct, gapless count"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_sweep_removes_only_elapsed_windows(pool: PgPool) {
    let now = Utc::now();
    let mut conn = pool.acquire().await.expect("connection");

    // One window that has elapsed, one that has not.
    rate_limit::hit(
        &mut conn,
        &[1u8; 32],
        10,
        ChronoDuration::minutes(1),
        now - ChronoDuration::hours(2),
    )
    .await
    .expect("old hit");
    rate_limit::hit(&mut conn, &[2u8; 32], 10, ChronoDuration::minutes(15), now)
        .await
        .expect("current hit");

    let removed = rate_limit::sweep_expired(&mut conn, now, 100)
        .await
        .expect("sweep");
    assert_eq!(removed, 1);

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM rate_limit_counters")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(remaining, 1, "the live window must survive");
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_sweep_is_bounded_by_its_batch_size(pool: PgPool) {
    let now = Utc::now();
    let mut conn = pool.acquire().await.expect("connection");

    for n in 0..10u8 {
        rate_limit::hit(
            &mut conn,
            &[n; 32],
            10,
            ChronoDuration::minutes(1),
            now - ChronoDuration::hours(2),
        )
        .await
        .expect("hit");
    }

    let removed = rate_limit::sweep_expired(&mut conn, now, 4)
        .await
        .expect("sweep");
    assert_eq!(removed, 4, "a sweep must not exceed its batch size");
}

#[sqlx::test(migrations = "../../migrations")]
async fn revoking_all_sessions_can_spare_the_calling_one(pool: PgPool) {
    let user_id = a_user(&pool, "sessions@example.test").await;
    let mut conn = pool.acquire().await.expect("connection");

    let mut ids = Vec::new();
    for n in 0..3u8 {
        let id = new_id();
        sessions::insert(
            &mut conn,
            id,
            user_id,
            &[n; 32],
            Utc::now() + ChronoDuration::days(1),
            Utc::now() + ChronoDuration::days(30),
            None,
            None,
        )
        .await
        .expect("insert session");
        ids.push(id);
    }

    let spared = ids[1];
    let revoked = sessions::revoke_all_for_user(
        &mut conn,
        user_id,
        RevocationReason::PasswordChange,
        Some(spared),
    )
    .await
    .expect("revoke");

    assert_eq!(revoked, 2);
    assert!(sessions::find_live(&mut conn, &[1u8; 32])
        .await
        .expect("find")
        .is_some());
    assert!(sessions::find_live(&mut conn, &[0u8; 32])
        .await
        .expect("find")
        .is_none());

    // Revoking again is a no-op rather than a double count.
    let again = sessions::revoke(&mut conn, ids[0], RevocationReason::Logout)
        .await
        .expect("revoke");
    assert!(!again, "an already-revoked session reports no change");
}

#[sqlx::test(migrations = "../../migrations")]
async fn granting_a_role_twice_is_reported_as_no_change(pool: PgPool) {
    let user_id = a_user(&pool, "roles@example.test").await;
    let mut conn = pool.acquire().await.expect("connection");

    assert!(users::grant_role(&mut conn, user_id, Role::Admin, None)
        .await
        .expect("grant"));
    assert!(
        !users::grant_role(&mut conn, user_id, Role::Admin, None)
            .await
            .expect("grant"),
        "a repeat grant must report no change so it writes no audit row"
    );

    assert_eq!(
        users::roles(&mut conn, user_id).await.expect("roles"),
        vec![Role::Admin]
    );

    assert!(users::revoke_role(&mut conn, user_id, Role::Admin)
        .await
        .expect("revoke"));
    assert!(!users::revoke_role(&mut conn, user_id, Role::Admin)
        .await
        .expect("revoke"));
    assert!(users::roles(&mut conn, user_id)
        .await
        .expect("roles")
        .is_empty());
}

/// Audit rows survive the account they describe.
#[sqlx::test(migrations = "../../migrations")]
async fn deleting_an_account_keeps_its_audit_trail(pool: PgPool) {
    let user_id = a_user(&pool, "gone@example.test").await;
    let mut conn = pool.acquire().await.expect("connection");

    audit::record(
        &mut conn,
        audit::AuditEvent::new("auth.registered", "users")
            .actor(audit::ActorKind::User, Some(user_id))
            .subject(user_id),
    )
    .await
    .expect("record");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("delete user");

    let row: (Option<uuid::Uuid>, String) =
        sqlx::query_as("SELECT actor_user_id, action FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("the audit row must remain");

    assert_eq!(row.0, None, "the actor reference is cleared, not cascaded");
    assert_eq!(row.1, "auth.registered");
}

/// Deleting an account does cascade to its credentials and sessions: those are
/// credential material and must not outlive the account.
#[sqlx::test(migrations = "../../migrations")]
async fn deleting_an_account_removes_its_credential_material(pool: PgPool) {
    let user_id = a_user(&pool, "cascade@example.test").await;
    let mut conn = pool.acquire().await.expect("connection");
    passwords::insert(&mut conn, user_id, "$argon2id$placeholder")
        .await
        .expect("credential");
    sessions::insert(
        &mut conn,
        new_id(),
        user_id,
        &[9u8; 32],
        Utc::now() + ChronoDuration::days(1),
        Utc::now() + ChronoDuration::days(30),
        None,
        None,
    )
    .await
    .expect("session");
    users::grant_role(&mut conn, user_id, Role::Homeowner, None)
        .await
        .expect("role");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("delete user");

    for table in ["password_credentials", "sessions", "user_roles"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 0, "{table} should have been cascaded");
    }
}
