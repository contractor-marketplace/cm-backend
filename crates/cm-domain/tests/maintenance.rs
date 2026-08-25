//! Retention.
//!
//! Three tables would otherwise grow forever. These tests prove that what is
//! deleted is only what nothing needs, that live data survives, and that a
//! single pass is bounded.

use chrono::{Duration, Utc};
use cm_db::repo::maintenance;
use cm_db::PgPool;
use cm_domain::maintenance::prune;

async fn a_user(pool: &PgPool, email: &str) -> uuid::Uuid {
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::insert(
        &mut conn,
        cm_core::new_id(),
        email,
        "Test",
        cm_db::repo::users::AccountType::Homeowner,
    )
    .await
    .expect("user")
    .id
}

/// Insert a session with explicit lifetimes, ageing the row where needed.
async fn a_session(pool: &PgPool, user_id: uuid::Uuid, token: u8, age_days: i64, revoked: bool) {
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::sessions::insert(
        &mut conn,
        cm_core::new_id(),
        user_id,
        &[token; 32],
        Utc::now() + Duration::days(1),
        Utc::now() + Duration::days(90),
        None,
        None,
    )
    .await
    .expect("session");

    if age_days > 0 || revoked {
        sqlx::query(
            "UPDATE sessions SET \
                 created_at = now() - make_interval(days => $2 + 100), \
                 absolute_expires_at = now() - make_interval(days => $2), \
                 idle_expires_at = now() - make_interval(days => $2), \
                 revoked_at = CASE WHEN $3 THEN now() - make_interval(days => $2) ELSE NULL END, \
                 revoked_reason = CASE WHEN $3 THEN 'logout' ELSE NULL END \
              WHERE token_hash = $1",
        )
        .bind(&[token; 32][..])
        .bind(age_days as i32)
        .bind(revoked)
        .execute(pool)
        .await
        .expect("age the session");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn pruning_removes_only_sessions_nothing_can_use(pool: PgPool) {
    let user_id = a_user(&pool, "retention@example.test").await;

    a_session(&pool, user_id, 1, 0, false).await; // live
    a_session(&pool, user_id, 2, 60, false).await; // long expired
    a_session(&pool, user_id, 3, 60, true).await; // long revoked
    a_session(&pool, user_id, 4, 5, false).await; // expired, still inside the grace period

    let pruned = prune(&pool, Utc::now(), 30, None).await.expect("prune");
    assert_eq!(pruned.sessions, 2, "only the two beyond the grace period");

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(remaining, 2);

    // The live session is untouched and still resolves.
    let mut conn = pool.acquire().await.expect("connection");
    assert!(
        cm_db::repo::sessions::find_live(&mut conn, &[1u8; 32])
            .await
            .expect("find")
            .is_some(),
        "a live session must never be pruned"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn pruning_leaves_queued_geocode_jobs_alone(pool: PgPool) {
    // A contractor to hang jobs from.
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::reference::seed_trades(&mut conn)
        .await
        .expect("trades");
    let run_id = cm_db::repo::licenses::begin_run(
        &mut conn,
        cm_db::repo::licenses::Source::CslbMasterList,
        "fixture.csv",
        &[3u8; 32],
        None,
    )
    .await
    .expect("run");

    let mut ids = Vec::new();
    for (n, status) in ["queued", "succeeded", "failed", "skipped"]
        .iter()
        .enumerate()
    {
        let record = cm_db::repo::licenses::LicenseRecord {
            license_no: format!("L{n}"),
            business_name: format!("Business {n}"),
            business_type: None,
            status: cm_db::repo::licenses::LicenseStatus::Active,
            status_raw: "CLEAR".into(),
            issue_date: None,
            expiration_date: None,
            classifications: vec![],
            bond_amount_cents: None,
            workers_comp_status: None,
            address_line1: None,
            city: None,
            state: None,
            postal_code: None,
            county: None,
            phone: None,
            raw: serde_json::json!({}),
            content_hash: vec![n as u8; 32],
        };
        let stored = cm_db::repo::licenses::upsert(&mut conn, run_id, &record)
            .await
            .expect("licence");
        let contractor = cm_db::repo::contractors::upsert_from_license(
            &mut conn,
            &cm_db::repo::contractors::SourceFacts {
                license_record_id: stored.id,
                display_name: record.business_name.clone(),
                slug: format!("business-{n}"),
                postal_code: None,
                region_id: None,
            },
        )
        .await
        .expect("contractor");

        cm_db::repo::geocode::enqueue(&mut conn, contractor.id, &[n as u8; 32])
            .await
            .expect("enqueue");
        sqlx::query(
            "UPDATE geocode_queue SET status = $2, updated_at = now() - interval '60 days' \
              WHERE contractor_id = $1",
        )
        .bind(contractor.id)
        .bind(status)
        .execute(&pool)
        .await
        .expect("age");
        ids.push(contractor.id);
    }
    drop(conn);

    let pruned = prune(&pool, Utc::now(), 30, None).await.expect("prune");
    assert_eq!(pruned.geocode_jobs, 3, "the three terminal states");

    let remaining: Vec<String> = sqlx::query_scalar("SELECT status FROM geocode_queue")
        .fetch_all(&pool)
        .await
        .expect("statuses");
    assert_eq!(
        remaining,
        vec!["queued"],
        "an outstanding job must never be pruned, however old"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_audit_log_is_kept_unless_a_retention_period_is_asked_for(pool: PgPool) {
    let user_id = a_user(&pool, "audited@example.test").await;
    let mut conn = pool.acquire().await.expect("connection");
    for _ in 0..3 {
        cm_db::repo::audit::record(
            &mut conn,
            cm_db::repo::audit::AuditEvent::new("auth.login_succeeded", "users")
                .actor(cm_db::repo::audit::ActorKind::User, Some(user_id))
                .subject(user_id),
        )
        .await
        .expect("audit");
    }
    sqlx::query("UPDATE audit_log SET created_at = now() - interval '400 days'")
        .execute(&pool)
        .await
        .expect("age");
    drop(conn);

    let pruned = prune(&pool, Utc::now(), 30, None).await.expect("prune");
    assert_eq!(
        pruned.audit_rows, 0,
        "deleting an audit trail is a policy decision, not housekeeping"
    );
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(kept, 3);

    // Only when explicitly asked.
    let pruned = prune(&pool, Utc::now(), 30, Some(365))
        .await
        .expect("prune");
    assert_eq!(pruned.audit_rows, 3);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_single_pass_is_bounded(pool: PgPool) {
    // A pass deletes at most BATCH * MAX_BATCHES rows, so a table left to grow
    // for a year is caught up over several nights rather than in one long lock.
    let ceiling = maintenance::BATCH * maintenance::MAX_BATCHES as i64;
    assert!(ceiling > 0);
    assert!(
        ceiling <= 500_000,
        "a single pass must stay well short of a table-sized transaction"
    );

    // And an empty database is a no-op rather than an error.
    let pruned = prune(&pool, Utc::now(), 30, Some(30)).await.expect("prune");
    assert_eq!(pruned, cm_db::repo::maintenance::Pruned::default());
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_growth_report_names_the_tables_that_grow(pool: PgPool) {
    let report = cm_domain::maintenance::growth_report(&pool)
        .await
        .expect("report");

    let names: Vec<&str> = report.iter().map(|(name, _)| name.as_str()).collect();
    for expected in [
        "sessions",
        "audit_log",
        "rate_limit_counters",
        "geocode_queue",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
}
