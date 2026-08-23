//! The geocoding worker and the location-privacy rule.

use cm_db::repo::contractors::{self, AddressVisibility, ProfileUpdate};
use cm_db::repo::{geocode, reference};
use cm_db::PgPool;
use cm_domain::geocode_worker::{self, WorkerConfig};
use cm_domain::geocoder::{Coordinates, GeocodeFuture, Geocoder, StaticGeocoder};
use cm_domain::import::{self, ImportOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Highland Park's centroid, and a street address inside it.
const HIGHLAND_PARK_CENTROID: (f64, f64) = (34.1156, -118.1926);
const IBARRA_ADDRESS: &str = "4210 York Blvd, Los Angeles, CA, 90042";
const IBARRA_TRUE_POINT: (f64, f64) = (34.1180, -118.1875);

async fn seed_and_import(pool: &PgPool) {
    let mut conn = pool.acquire().await.expect("connection");
    reference::seed_trades(&mut conn).await.expect("trades");

    let mut reader = csv::Reader::from_path(fixture("zcta_la_sample.csv")).expect("fixture");
    for row in reader.records() {
        let row = row.expect("row");
        reference::upsert_zcta(
            &mut conn,
            row.get(0).expect("code"),
            row.get(1).expect("name"),
            row.get(2).expect("lat").parse().expect("lat"),
            row.get(3).expect("lon").parse().expect("lon"),
            "test",
        )
        .await
        .expect("zcta");
    }
    drop(conn);

    import::run(
        pool,
        &ImportOptions {
            source: cm_db::repo::licenses::Source::CslbMasterList,
            file_path: fixture("cslb_sample.csv"),
            county: Some("LOS ANGELES".to_owned()),
            snapshot_date: None,
            batch_size: 10,
            dry_run: false,
        },
    )
    .await
    .expect("import");
}

async fn contractor_by_licence(pool: &PgPool, license_no: &str) -> uuid::Uuid {
    sqlx::query_scalar(
        "SELECT c.id FROM contractors c JOIN license_records l ON l.id = c.license_record_id \
          WHERE l.license_no = $1",
    )
    .bind(license_no)
    .fetch_one(pool)
    .await
    .expect("contractor")
}

fn static_geocoder() -> Arc<dyn Geocoder> {
    let mut answers = HashMap::new();
    answers.insert(
        IBARRA_ADDRESS.to_owned(),
        Coordinates {
            lat: IBARRA_TRUE_POINT.0,
            lon: IBARRA_TRUE_POINT.1,
        },
    );
    Arc::new(StaticGeocoder::new(answers))
}

async fn published_point(
    pool: &PgPool,
    contractor_id: uuid::Uuid,
) -> (Option<f64>, Option<f64>, String) {
    sqlx::query_as(
        "SELECT ST_Y(public_point::geometry), ST_X(public_point::geometry), public_point_source \
           FROM contractors WHERE id = $1",
    )
    .bind(contractor_id)
    .fetch_one(pool)
    .await
    .expect("point")
}

/// The rule: a protected listing keeps its centroid even once located exactly.
#[sqlx::test(migrations = "../../migrations")]
async fn a_protected_listing_publishes_its_centroid_not_its_address(pool: PgPool) {
    seed_and_import(&pool).await;
    let contractor_id = contractor_by_licence(&pool, "1047382").await;

    let stats = geocode_worker::run_once(&pool, &static_geocoder(), &WorkerConfig::default())
        .await
        .expect("worker pass");
    assert!(stats.located >= 1);

    let (lat, lon, source) = published_point(&pool, contractor_id).await;
    assert_eq!(source, "zip_centroid");
    assert!((lat.expect("lat") - HIGHLAND_PARK_CENTROID.0).abs() < 1e-6);
    assert!((lon.expect("lon") - HIGHLAND_PARK_CENTROID.1).abs() < 1e-6);

    // The exact point is stored, and is not what anyone is shown.
    let has_precise: bool =
        sqlx::query_scalar("SELECT precise_point IS NOT NULL FROM contractors WHERE id = $1")
            .bind(contractor_id)
            .fetch_one(&pool)
            .await
            .expect("precise");
    assert!(has_precise);
}

/// The attack the rule exists to stop.
///
/// If search ran against the precise point while the map published a centroid,
/// an attacker could shrink the radius around a guessed address until the
/// contractor dropped out, and so recover the address. Because search reads the
/// published point, a tight ring on the true address finds nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_protected_address_cannot_be_triangulated_through_the_radius_filter(pool: PgPool) {
    seed_and_import(&pool).await;
    let contractor_id = contractor_by_licence(&pool, "1047382").await;
    geocode_worker::run_once(&pool, &static_geocoder(), &WorkerConfig::default())
        .await
        .expect("worker pass");

    let mut conn = pool.acquire().await.expect("connection");

    // A 200 m ring centred exactly on the true address.
    let near_truth = cm_db::repo::search::Near {
        lat: IBARRA_TRUE_POINT.0,
        lon: IBARRA_TRUE_POINT.1,
        radius_m: 200.0,
    };
    let found = cm_db::repo::search::list(
        &mut conn,
        &cm_db::repo::search::Filters {
            near: Some(near_truth),
            ..Default::default()
        },
        cm_db::repo::search::Sort::Distance,
        50,
        None,
    )
    .await
    .expect("search");

    assert!(
        !found.contractors.iter().any(|c| c.id == contractor_id),
        "a ring on the true address must not single the contractor out"
    );

    // The same ring on the published centroid does find it — the contractor is
    // searchable, just at ZIP precision.
    let near_centroid = cm_db::repo::search::Near {
        lat: HIGHLAND_PARK_CENTROID.0,
        lon: HIGHLAND_PARK_CENTROID.1,
        radius_m: 200.0,
    };
    let found = cm_db::repo::search::list(
        &mut conn,
        &cm_db::repo::search::Filters {
            near: Some(near_centroid),
            ..Default::default()
        },
        cm_db::repo::search::Sort::Distance,
        50,
        None,
    )
    .await
    .expect("search");

    assert!(found.contractors.iter().any(|c| c.id == contractor_id));
}

/// A claimant who opts in publishes their exact point.
#[sqlx::test(migrations = "../../migrations")]
async fn a_claimed_listing_that_opts_in_publishes_its_exact_point(pool: PgPool) {
    seed_and_import(&pool).await;
    let contractor_id = contractor_by_licence(&pool, "1047382").await;

    let user_id = cm_core::new_id();
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::insert(&mut conn, user_id, "owner@example.test", "Owner")
        .await
        .expect("user");
    contractors::attach_claimant(&mut conn, contractor_id, user_id)
        .await
        .expect("claim");
    contractors::update_profile(
        &mut conn,
        contractor_id,
        &ProfileUpdate {
            address_visibility: Some(AddressVisibility::Public),
            ..Default::default()
        },
    )
    .await
    .expect("publish the address");
    drop(conn);

    geocode_worker::run_once(&pool, &static_geocoder(), &WorkerConfig::default())
        .await
        .expect("worker pass");

    let (lat, lon, source) = published_point(&pool, contractor_id).await;
    assert_eq!(source, "exact");
    assert!((lat.expect("lat") - IBARRA_TRUE_POINT.0).abs() < 1e-6);
    assert!((lon.expect("lon") - IBARRA_TRUE_POINT.1).abs() < 1e-6);

    // Turning it back off must take effect immediately.
    let mut tx = pool.begin().await.expect("tx");
    contractors::update_profile(
        &mut tx,
        contractor_id,
        &ProfileUpdate {
            address_visibility: Some(AddressVisibility::Protected),
            ..Default::default()
        },
    )
    .await
    .expect("protect");
    cm_domain::location::reapply(&mut tx, contractor_id)
        .await
        .expect("reapply");
    tx.commit().await.expect("commit");

    let (_, _, source) = published_point(&pool, contractor_id).await;
    assert_eq!(
        source, "zip_centroid",
        "withdrawing consent takes effect at once"
    );
}

/// A provider that never answers must not lose the contractor from the map.
#[sqlx::test(migrations = "../../migrations")]
async fn a_failing_provider_backs_off_and_leaves_the_centroid_intact(pool: PgPool) {
    seed_and_import(&pool).await;
    let contractor_id = contractor_by_licence(&pool, "1047382").await;

    struct Broken(Arc<AtomicUsize>);
    impl Geocoder for Broken {
        fn name(&self) -> &'static str {
            "broken"
        }
        fn locate(&self, _address: String) -> GeocodeFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(cm_core::AppError::unavailable("provider is down")) })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let broken: Arc<dyn Geocoder> = Arc::new(Broken(calls.clone()));

    let stats = geocode_worker::run_once(
        &pool,
        &broken,
        &WorkerConfig {
            rate_per_second: 1000.0,
            ..Default::default()
        },
    )
    .await
    .expect("worker pass");

    assert!(stats.failed >= 1);
    assert!(calls.load(Ordering::SeqCst) >= 1);

    // Retried later, not immediately.
    let mut conn = pool.acquire().await.expect("connection");
    let (status, attempts, next_attempt) = geocode::status_of(&mut conn, contractor_id)
        .await
        .expect("status")
        .expect("a job");
    assert_eq!(status, "queued", "still retryable");
    assert_eq!(attempts, 1);
    assert!(next_attempt > chrono::Utc::now(), "backed off");

    // And the contractor is still on the map at ZIP precision.
    let (_, _, source) = published_point(&pool, contractor_id).await;
    assert_eq!(source, "zip_centroid");
}

#[sqlx::test(migrations = "../../migrations")]
async fn attempts_are_capped_so_a_dead_address_stops_being_retried(pool: PgPool) {
    seed_and_import(&pool).await;
    let contractor_id = contractor_by_licence(&pool, "1047382").await;

    struct Broken;
    impl Geocoder for Broken {
        fn name(&self) -> &'static str {
            "broken"
        }
        fn locate(&self, _address: String) -> GeocodeFuture {
            Box::pin(async { Err(cm_core::AppError::unavailable("down")) })
        }
    }
    let broken: Arc<dyn Geocoder> = Arc::new(Broken);
    let config = WorkerConfig {
        max_attempts: 2,
        rate_per_second: 1000.0,
        ..Default::default()
    };

    for _ in 0..config.max_attempts {
        // Make the job due again without waiting out the backoff.
        sqlx::query("UPDATE geocode_queue SET next_attempt_at = now() - interval '1 hour'")
            .execute(&pool)
            .await
            .expect("make due");
        geocode_worker::run_once(&pool, &broken, &config)
            .await
            .expect("pass");
    }

    let mut conn = pool.acquire().await.expect("connection");
    let (status, attempts, _) = geocode::status_of(&mut conn, contractor_id)
        .await
        .expect("status")
        .expect("a job");
    assert_eq!(status, "failed", "a capped job stops being retried forever");
    assert_eq!(attempts, config.max_attempts);
}

/// Two workers must not do the same job twice.
#[sqlx::test(migrations = "../../migrations")]
async fn two_workers_never_claim_the_same_job(pool: PgPool) {
    seed_and_import(&pool).await;

    let claim = |worker: &'static str| {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut tx = pool.begin().await.expect("tx");
            let jobs = geocode::claim(&mut tx, worker, 6).await.expect("claim");
            tx.commit().await.expect("commit");
            jobs.into_iter().map(|job| job.id).collect::<Vec<_>>()
        })
    };

    let (first, second) = tokio::join!(claim("worker-a"), claim("worker-b"));
    let first = first.expect("join");
    let second = second.expect("join");

    let overlap: Vec<_> = first.iter().filter(|id| second.contains(id)).collect();
    assert!(
        overlap.is_empty(),
        "SKIP LOCKED must prevent double claims: {overlap:?}"
    );
    assert_eq!(
        first.len() + second.len(),
        6,
        "between them they take every job, once"
    );
}

/// A worker that dies mid-job must not leak queue capacity.
#[sqlx::test(migrations = "../../migrations")]
async fn a_job_abandoned_by_a_dead_worker_is_returned_to_the_queue(pool: PgPool) {
    seed_and_import(&pool).await;

    let mut conn = pool.acquire().await.expect("connection");
    let claimed = geocode::claim(&mut conn, "worker-that-died", 3)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 3);

    // Nothing would ever claim these again without the sweep.
    let requeued = geocode::requeue_stalled(&mut conn, 600)
        .await
        .expect("sweep");
    assert_eq!(requeued, 0, "a recent claim is not yet stale");

    sqlx::query(
        "UPDATE geocode_queue SET locked_at = now() - interval '2 hours' \
                  WHERE status = 'in_progress'",
    )
    .execute(&pool)
    .await
    .expect("age the claim");

    let requeued = geocode::requeue_stalled(&mut conn, 600)
        .await
        .expect("sweep");
    assert_eq!(requeued, 3);

    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM geocode_queue WHERE status = 'queued'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(queued, 6);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_address_the_provider_cannot_match_is_not_retried_forever(pool: PgPool) {
    seed_and_import(&pool).await;

    // The static geocoder knows one address; the rest come back not-found.
    let stats = geocode_worker::run_once(
        &pool,
        &static_geocoder(),
        &WorkerConfig {
            rate_per_second: 1000.0,
            ..Default::default()
        },
    )
    .await
    .expect("pass");

    assert_eq!(stats.located, 1);
    assert_eq!(stats.not_found, 5);

    let requeued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM geocode_queue WHERE status = 'queued'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(
        requeued, 0,
        "a definitive no-match is not a retryable failure"
    );
}

/// Unlocated contractors are silently absent from distance search, so the count
/// is an operational signal rather than something to discover from a support
/// ticket.
#[sqlx::test(migrations = "../../migrations")]
async fn the_unlocated_count_is_observable(pool: PgPool) {
    seed_and_import(&pool).await;
    let mut conn = pool.acquire().await.expect("connection");

    assert_eq!(
        geocode::unlocated_contractor_count(&mut conn)
            .await
            .expect("count"),
        0,
        "the ZIP centroid locates everything the fixture covers"
    );

    // A contractor in a ZIP with no known centroid has no pin.
    sqlx::query(
        "UPDATE contractors SET public_point = NULL, public_point_source = 'none' \
                  WHERE postal_code = '90042'",
    )
    .execute(&pool)
    .await
    .expect("clear");

    assert_eq!(
        geocode::unlocated_contractor_count(&mut conn)
            .await
            .expect("count"),
        1
    );
}
