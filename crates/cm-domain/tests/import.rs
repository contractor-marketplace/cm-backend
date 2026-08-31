//! The CSLB importer.
//!
//! The fixture mirrors the documented shape of the CSLB master list. It is
//! **not** a real download — the exact column titles have not been verified
//! against one in this environment — so the header mapping is tolerant and
//! fails loudly rather than guessing. That limitation is recorded in the
//! handover notes; everything else here is exercised for real.

use cm_db::repo::licenses::Source;
use cm_db::PgPool;
use cm_domain::import::{self, ImportOptions};
use sqlx::Row;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn options(file: PathBuf) -> ImportOptions {
    ImportOptions {
        source: Source::CslbMasterList,
        file_path: file,
        county: Some("LOS ANGELES".to_owned()),
        snapshot_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1),
        batch_size: 2,
        dry_run: false,
    }
}

/// Reference data the importer maps against.
async fn seed(pool: &PgPool) {
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::reference::seed_trades(&mut conn)
        .await
        .expect("seed trades");

    let mut reader = csv::Reader::from_path(fixture("zcta_la_sample.csv")).expect("zcta fixture");
    for row in reader.records() {
        let row = row.expect("row");
        cm_db::repo::reference::upsert_zcta(
            &mut conn,
            row.get(0).expect("code"),
            row.get(1).expect("name"),
            row.get(2).expect("lat").parse().expect("lat"),
            row.get(3).expect("lon").parse().expect("lon"),
            None,
            "test",
        )
        .await
        .expect("upsert zcta");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_import_creates_licences_contractors_and_trades(pool: PgPool) {
    seed(&pool).await;

    let counts = import::run(&pool, &options(fixture("cslb_sample.csv")))
        .await
        .expect("import");

    assert_eq!(counts.read, 7);
    assert_eq!(counts.inserted, 6, "six LA County rows");
    assert_eq!(counts.skipped, 1, "the Fresno row is out of county");
    assert_eq!(counts.rejected, 0);

    let licences: i64 = sqlx::query_scalar("SELECT count(*) FROM license_records")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(licences, 6);

    let contractors: i64 = sqlx::query_scalar("SELECT count(*) FROM contractors")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(contractors, 6);

    // Classifications became trades.
    let trades: Vec<String> = sqlx::query_scalar(
        "SELECT t.slug FROM contractor_trades ct \
           JOIN trades t ON t.id = ct.trade_id \
           JOIN contractors c ON c.id = ct.contractor_id \
           JOIN license_records l ON l.id = c.license_record_id \
          WHERE l.license_no = '1047382' ORDER BY t.slug",
    )
    .fetch_all(&pool)
    .await
    .expect("trades");
    assert_eq!(trades, vec!["general-contractor", "painter"]);
}

/// The property the whole design turns on.
#[sqlx::test(migrations = "../../migrations")]
async fn importing_the_same_file_twice_changes_nothing(pool: PgPool) {
    seed(&pool).await;
    let options = options(fixture("cslb_sample.csv"));

    import::run(&pool, &options).await.expect("first import");

    let before: Vec<(String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT license_no, updated_at FROM license_records ORDER BY license_no")
            .fetch_all(&pool)
            .await
            .expect("snapshot");
    let contractors_before: Vec<(uuid::Uuid, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT id, updated_at FROM contractors ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("snapshot");

    // The same bytes are refused outright, which is the first line of defence.
    let refused = import::run(&pool, &options).await.expect_err("same bytes");
    assert_eq!(refused.code(), "conflict");

    // Copy the file so the digest differs but the content does not — this is
    // the case that proves row-level idempotency rather than file-level.
    let copy = std::env::temp_dir().join(format!("cslb-copy-{}.csv", std::process::id()));
    let mut body = std::fs::read_to_string(fixture("cslb_sample.csv")).expect("read");
    body.push('\n'); // differs by one byte; every row is identical
    std::fs::write(&copy, body).expect("write");

    let counts = import::run(&pool, &options_for(&copy))
        .await
        .expect("re-import");
    assert_eq!(counts.inserted, 0);
    assert_eq!(counts.updated, 0);
    assert_eq!(counts.unchanged, 6, "every row is unchanged");

    let after: Vec<(String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT license_no, updated_at FROM license_records ORDER BY license_no")
            .fetch_all(&pool)
            .await
            .expect("snapshot");
    assert_eq!(before, after, "an unchanged row must not be rewritten");

    let contractors_after: Vec<(uuid::Uuid, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT id, updated_at FROM contractors ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("snapshot");
    assert_eq!(contractors_before, contractors_after);

    let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM license_record_versions")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(versions, 6, "no second version row for unchanged content");

    std::fs::remove_file(copy).ok();
}

fn options_for(file: &Path) -> ImportOptions {
    ImportOptions {
        file_path: file.to_path_buf(),
        ..options(file.to_path_buf())
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_changed_row_produces_one_update_and_one_new_version(pool: PgPool) {
    seed(&pool).await;
    import::run(&pool, &options(fixture("cslb_sample.csv")))
        .await
        .expect("first import");

    // One licence changes status; everything else is byte-identical.
    let body = std::fs::read_to_string(fixture("cslb_sample.csv"))
        .expect("read")
        .replace(
            "983311,Meridian Electric Co,Corporation,CLEAR",
            "983311,Meridian Electric Co,Corporation,EXPIRED",
        );
    let changed = std::env::temp_dir().join(format!("cslb-changed-{}.csv", std::process::id()));
    std::fs::write(&changed, body).expect("write");

    let counts = import::run(&pool, &options_for(&changed))
        .await
        .expect("import");
    assert_eq!(counts.updated, 1);
    assert_eq!(counts.unchanged, 5);
    assert_eq!(counts.inserted, 0);

    let versions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM license_record_versions v \
           JOIN license_records l ON l.id = v.license_record_id \
          WHERE l.license_no = '983311'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(versions, 2, "the raw history must keep both observations");

    std::fs::remove_file(changed).ok();
}

/// The rule that makes a refresh safe to run against live data.
#[sqlx::test(migrations = "../../migrations")]
async fn an_import_never_overwrites_what_a_claimant_wrote(pool: PgPool) {
    seed(&pool).await;
    import::run(&pool, &options(fixture("cslb_sample.csv")))
        .await
        .expect("first import");

    let contractor_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT c.id FROM contractors c JOIN license_records l ON l.id = c.license_record_id \
          WHERE l.license_no = '1047382'",
    )
    .fetch_one(&pool)
    .await
    .expect("contractor");

    // Someone claims it and writes a profile.
    let user_id = cm_core::new_id();
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::insert(
        &mut conn,
        user_id,
        "owner@example.test",
        "Owner",
        cm_db::repo::users::AccountType::Contractor,
    )
    .await
    .expect("user");
    cm_db::repo::contractors::attach_claimant(&mut conn, contractor_id, user_id)
        .await
        .expect("claim");
    sqlx::query(
        "UPDATE contractors SET bio = 'Second-generation GC', accepts_dm = true, \
                display_name = 'Ibarra and Daughters' WHERE id = $1",
    )
    .bind(contractor_id)
    .execute(&pool)
    .await
    .expect("profile");

    // A refresh with a changed source name must not take the display name back.
    let body = std::fs::read_to_string(fixture("cslb_sample.csv"))
        .expect("read")
        .replace(
            "Ibarra & Daughters Construction",
            "IBARRA AND DAUGHTERS CONSTRUCTION INC",
        );
    let refreshed = std::env::temp_dir().join(format!("cslb-refresh-{}.csv", std::process::id()));
    std::fs::write(&refreshed, body).expect("write");
    import::run(&pool, &options_for(&refreshed))
        .await
        .expect("import");

    let row = sqlx::query("SELECT display_name, bio, accepts_dm FROM contractors WHERE id = $1")
        .bind(contractor_id)
        .fetch_one(&pool)
        .await
        .expect("row");

    assert_eq!(row.get::<String, _>("display_name"), "Ibarra and Daughters");
    assert_eq!(
        row.get::<Option<String>, _>("bio").as_deref(),
        Some("Second-generation GC")
    );
    assert!(row.get::<bool, _>("accepts_dm"));

    std::fs::remove_file(refreshed).ok();
}

#[sqlx::test(migrations = "../../migrations")]
async fn statuses_drive_the_verified_badge_and_inactive_is_not_active(pool: PgPool) {
    seed(&pool).await;
    import::run(&pool, &options(fixture("cslb_sample.csv")))
        .await
        .expect("import");

    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT license_no, status FROM license_records ORDER BY license_no")
            .fetch_all(&pool)
            .await
            .expect("statuses");

    let by_licence: std::collections::HashMap<_, _> = statuses.into_iter().collect();
    assert_eq!(by_licence["1047382"], "active");
    assert_eq!(by_licence["445190"], "expired");
    assert_eq!(
        by_licence["902276"], "inactive",
        "INACTIVE contains ACTIVE; mapping it to active would mark dead licences live"
    );
    assert_eq!(by_licence["618842"], "suspended");

    // Nothing is verified: no listing has been claimed.
    let verified: i64 = sqlx::query_scalar("SELECT count(*) FROM contractors WHERE verified")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(verified, 0, "a licence alone never verifies a listing");
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_contractor_is_located_at_zip_precision_and_queued_for_geocoding(pool: PgPool) {
    seed(&pool).await;
    import::run(&pool, &options(fixture("cslb_sample.csv")))
        .await
        .expect("import");

    let located: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM contractors \
          WHERE public_point IS NOT NULL AND public_point_source = 'zip_centroid'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        located, 6,
        "a ZIP centroid makes a contractor searchable at once"
    );

    // And none has a precise point yet: nothing has been geocoded.
    let precise: i64 =
        sqlx::query_scalar("SELECT count(*) FROM contractors WHERE precise_point IS NOT NULL")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(precise, 0);

    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM geocode_queue WHERE status = 'queued'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(queued, 6);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_malformed_or_headerless_file_is_refused_or_counted(pool: PgPool) {
    seed(&pool).await;

    // Wrong headers: refused outright, naming what it saw.
    let wrong = std::env::temp_dir().join(format!("cslb-wrong-{}.csv", std::process::id()));
    std::fs::write(&wrong, "alpha,beta\n1,2\n").expect("write");
    let error = import::run(&pool, &options_for(&wrong))
        .await
        .expect_err("refused");
    assert!(error.to_string().contains("license_no"), "{error}");

    // A row missing its licence number is counted as rejected, not fatal.
    let partial = std::env::temp_dir().join(format!("cslb-partial-{}.csv", std::process::id()));
    std::fs::write(
        &partial,
        "LicenseNo,BusinessName,PrimaryStatus,County\n\
         ,No Licence Number,CLEAR,LOS ANGELES\n\
         777777,Fine Builders,CLEAR,LOS ANGELES\n",
    )
    .expect("write");
    let counts = import::run(&pool, &options_for(&partial))
        .await
        .expect("import");
    assert_eq!(counts.rejected, 1);
    assert_eq!(counts.inserted, 1);

    std::fs::remove_file(wrong).ok();
    std::fs::remove_file(partial).ok();
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_dry_run_writes_nothing_and_does_not_consume_the_file(pool: PgPool) {
    seed(&pool).await;

    let mut dry = options(fixture("cslb_sample.csv"));
    dry.dry_run = true;
    let counts = import::run(&pool, &dry).await.expect("dry run");
    assert_eq!(counts.read, 7);

    let licences: i64 = sqlx::query_scalar("SELECT count(*) FROM license_records")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(licences, 0, "a dry run writes nothing");

    // And the real import afterwards is not refused as a duplicate.
    let counts = import::run(&pool, &options(fixture("cslb_sample.csv")))
        .await
        .expect("real import");
    assert_eq!(counts.inserted, 6);
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_raw_source_row_is_preserved_verbatim(pool: PgPool) {
    seed(&pool).await;
    import::run(&pool, &options(fixture("cslb_sample.csv")))
        .await
        .expect("import");

    let raw: serde_json::Value =
        sqlx::query_scalar("SELECT raw FROM license_records WHERE license_no = '1047382'")
            .fetch_one(&pool)
            .await
            .expect("raw");

    assert_eq!(raw["BusinessName"], "Ibarra & Daughters Construction");
    assert_eq!(raw["County"], "LOS ANGELES");
    assert_eq!(
        raw["BondAmount"], "$25,000.00",
        "the source string is kept exactly, not the parsed value"
    );
}
