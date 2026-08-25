//! Migration and schema-invariant tests.
//!
//! These run against a real PostgreSQL 16 with PostGIS. There is no mocked
//! database anywhere in this suite on purpose: partial unique indexes, CHECK
//! constraints and PostGIS types are exactly the things a mock cannot
//! reproduce, and they are the things carrying the guarantees.

use cm_db::migrate::{self, MigrationStatus};
use sqlx::PgPool;
use sqlx::Row;

/// Applying to a database that has never been migrated.
#[sqlx::test(migrations = false)]
async fn migrations_apply_to_an_empty_database(pool: PgPool) {
    let before = migrate::status(&pool).await.expect("status");
    assert_eq!(before.applied, None, "fixture should start unmigrated");
    assert!(!before.is_up_to_date());

    migrate::run(&pool).await.expect("migrations should apply");

    let after = migrate::status(&pool).await.expect("status");
    assert_eq!(
        after,
        MigrationStatus {
            applied: Some(migrate::embedded_version()),
            embedded: migrate::embedded_version(),
            dirty: Vec::new(),
        }
    );
    assert!(after.is_up_to_date());
}

/// Applying to a database that is already up to date.
#[sqlx::test(migrations = "../../migrations")]
async fn re_running_migrations_is_a_no_op(pool: PgPool) {
    let before = migrate::status(&pool).await.expect("status");
    assert!(before.is_up_to_date());

    let applied_at_before: Vec<(i64, i64)> = applied_rows(&pool).await;

    migrate::run(&pool).await.expect("re-run should succeed");

    let after = migrate::status(&pool).await.expect("status");
    assert_eq!(before, after);
    assert_eq!(
        applied_at_before,
        applied_rows(&pool).await,
        "a re-run must not rewrite the migration ledger"
    );
}

/// Editing an already-applied migration must fail loudly, not silently diverge.
#[sqlx::test(migrations = "../../migrations")]
async fn an_edited_migration_is_rejected(pool: PgPool) {
    let dir = std::env::temp_dir().join(format!("cm-migrate-checksum-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    for entry in std::fs::read_dir(&source).expect("read migrations") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        let mut body = std::fs::read_to_string(entry.path()).expect("read migration");
        if name.to_string_lossy().starts_with("0001") {
            body.push_str("\n-- an unauthorised edit to an applied migration\n");
        }
        std::fs::write(dir.join(name), body).expect("write migration");
    }

    let tampered = sqlx::migrate::Migrator::new(dir.as_path())
        .await
        .expect("build migrator");
    let error = tampered
        .run(&pool)
        .await
        .expect_err("an edited migration must be refused");

    assert!(
        matches!(error, sqlx::migrate::MigrateError::VersionMismatch(1)),
        "expected a checksum mismatch on version 1, got: {error:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// PostGIS, pg_trgm and unaccent are installed; nothing else is.
#[sqlx::test(migrations = "../../migrations")]
async fn only_the_expected_extensions_are_installed(pool: PgPool) {
    let installed: Vec<String> =
        sqlx::query_scalar("SELECT extname::text FROM pg_extension ORDER BY extname")
            .fetch_all(&pool)
            .await
            .expect("query extensions");

    assert_eq!(
        installed,
        vec!["pg_trgm", "plpgsql", "postgis", "unaccent"],
        "the extension set is part of the reviewed schema; citext in particular \
         is deliberately absent"
    );
}

/// The text-search configuration exists and actually folds accents, which is
/// the whole reason it exists rather than plain `english`.
#[sqlx::test(migrations = "../../migrations")]
async fn the_text_search_configuration_folds_accents(pool: PgPool) {
    let vector: String =
        sqlx::query_scalar("SELECT to_tsvector('public.english_unaccent', $1)::text")
            .bind("Íbarra & Daughters Construcción")
            .fetch_one(&pool)
            .await
            .expect("to_tsvector");

    assert!(
        vector.contains("ibarra"),
        "accents were not folded: {vector}"
    );
    assert!(!vector.contains("íbarra"), "accents survived: {vector}");

    // Immutability is what allows this configuration in a generated column and
    // an index later; a non-constant configuration would be rejected there.
    sqlx::query(
        "CREATE TABLE tsv_probe (id uuid PRIMARY KEY, name text NOT NULL, \
         doc tsvector GENERATED ALWAYS AS (to_tsvector('public.english_unaccent', name)) STORED)",
    )
    .execute(&pool)
    .await
    .expect("a generated tsvector column must be accepted");
}

/// The geography columns are usable and their GiST indexes exist.
#[sqlx::test(migrations = "../../migrations")]
async fn regions_support_indexed_geography_queries(pool: PgPool) {
    let highland_park = cm_core::new_id();
    sqlx::query(
        "INSERT INTO regions (id, kind, code, name, centroid, source) \
         VALUES ($1, 'zcta', '90042', 'Highland Park', \
                 ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography, 'test')",
    )
    .bind(highland_park)
    .bind(-118.1926_f64)
    .bind(34.1156_f64)
    .execute(&pool)
    .await
    .expect("insert region");

    // Selected as two doubles, never as a geography value: sqlx cannot decode
    // PostGIS types, and a query that returns one fails at the boundary.
    let row = sqlx::query(
        "SELECT ST_Y(centroid::geometry) AS lat, ST_X(centroid::geometry) AS lon, \
                ST_DWithin(centroid, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, 2000) AS near \
         FROM regions WHERE id = $3",
    )
    .bind(-118.20_f64)
    .bind(34.12_f64)
    .bind(highland_park)
    .fetch_one(&pool)
    .await
    .expect("distance query");

    assert!((row.get::<f64, _>("lat") - 34.1156).abs() < 1e-9);
    assert!((row.get::<f64, _>("lon") + 118.1926).abs() < 1e-9);
    assert!(row.get::<bool, _>("near"));

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexname::text FROM pg_indexes WHERE tablename = 'regions' ORDER BY indexname",
    )
    .fetch_all(&pool)
    .await
    .expect("query indexes");
    assert!(
        indexes.contains(&"regions_centroid_gix".to_owned()),
        "{indexes:?}"
    );
    assert!(
        indexes.contains(&"regions_boundary_gix".to_owned()),
        "{indexes:?}"
    );
}

/// An unindexed foreign key turns a parent delete into a sequential scan while
/// holding locks. Every FK must have an index leading with its column.
#[sqlx::test(migrations = "../../migrations")]
async fn every_foreign_key_has_a_supporting_index(pool: PgPool) {
    let multi_column: Vec<String> = sqlx::query_scalar(
        "SELECT conname::text FROM pg_constraint \
         WHERE contype = 'f' AND connamespace = 'public'::regnamespace \
           AND array_length(conkey, 1) > 1",
    )
    .fetch_all(&pool)
    .await
    .expect("query composite fks");
    assert!(
        multi_column.is_empty(),
        "this check only covers single-column foreign keys; extend it before adding {multi_column:?}"
    );

    let unindexed: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.conrelid::regclass::text, c.conname::text \
         FROM pg_constraint c \
         WHERE c.contype = 'f' AND c.connamespace = 'public'::regnamespace \
           AND NOT EXISTS ( \
             SELECT 1 FROM pg_index i \
             WHERE i.indrelid = c.conrelid AND i.indkey[0] = c.conkey[1] \
           ) \
         ORDER BY 1, 2",
    )
    .fetch_all(&pool)
    .await
    .expect("query unindexed fks");

    assert!(
        unindexed.is_empty(),
        "foreign keys without an index: {unindexed:?}"
    );
}

/// Primary keys are UUIDv7 generated in Rust. A database-side default would
/// silently hand out v4s and lose the time ordering.
#[sqlx::test(migrations = "../../migrations")]
async fn uuid_primary_keys_have_no_database_default(pool: PgPool) {
    let defaulted: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, a.attname::text \
         FROM pg_class c \
         JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary \
         JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = ANY(i.indkey) \
         JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum \
         WHERE c.relnamespace = 'public'::regnamespace AND a.atttypid = 'uuid'::regtype \
         ORDER BY 1, 2",
    )
    .fetch_all(&pool)
    .await
    .expect("query pk defaults");

    assert!(
        defaulted.is_empty(),
        "uuid primary keys must have no DEFAULT; found: {defaulted:?}"
    );
}

/// The timestamp convention from the data-model document, enforced.
///
/// One documented exception: `audit_log` is append-only, so an `updated_at`
/// column would be a field that can only ever lie. The exception is listed here
/// rather than weakening the rule, so adding a second one is a visible change.
#[sqlx::test(migrations = "../../migrations")]
async fn every_table_carries_created_at_and_updated_at(pool: PgPool) {
    const EXEMPT: &[(&str, &str)] = &[("audit_log", "updated_at")];

    let missing: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, col.col \
         FROM pg_class c \
         CROSS JOIN (VALUES ('created_at'), ('updated_at')) AS col(col) \
         WHERE c.relkind = 'r' AND c.relnamespace = 'public'::regnamespace \
           AND c.relname NOT LIKE '\\_sqlx%' \
           AND c.relname <> 'spatial_ref_sys' \
           AND NOT EXISTS ( \
             SELECT 1 FROM pg_attribute a \
             WHERE a.attrelid = c.oid AND a.attname = col.col AND NOT a.attisdropped \
           ) \
         ORDER BY 1, 2",
    )
    .fetch_all(&pool)
    .await
    .expect("query timestamp columns");

    let unexpected: Vec<_> = missing
        .into_iter()
        .filter(|(table, column)| !EXEMPT.contains(&(table.as_str(), column.as_str())))
        .collect();

    assert!(
        unexpected.is_empty(),
        "tables missing timestamp columns: {unexpected:?}"
    );
}

/// Status-like columns are TEXT + CHECK rather than native enums, so the
/// allowed literals live in the catalogue. This asserts them explicitly; when
/// the matching Rust enum arrives it is compared against this same query.
#[sqlx::test(migrations = "../../migrations")]
async fn region_kind_allows_exactly_the_documented_values(pool: PgPool) {
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) \
         FROM pg_constraint c \
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) \
         WHERE c.contype = 'c' AND c.conrelid = 'regions'::regclass AND a.attname = 'kind'",
    )
    .fetch_one(&pool)
    .await
    .expect("query check constraint");

    let mut literals = literals_in(&definition);
    literals.sort_unstable();
    assert_eq!(
        literals,
        vec!["city", "county", "neighborhood", "zcta"],
        "constraint definition was: {definition}"
    );
}

/// The account-type vocabulary in the database and in Rust must agree. They
/// are two hand-written lists, and a value in one but not the other is exactly
/// the kind of drift that shows up as a 500 in production rather than a
/// compile error.
#[sqlx::test(migrations = "../../migrations")]
async fn account_type_matches_the_rust_enum(pool: PgPool) {
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) \
         FROM pg_constraint c \
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) \
         WHERE c.contype = 'c' AND c.conrelid = 'users'::regclass \
           AND a.attname = 'account_type'",
    )
    .fetch_one(&pool)
    .await
    .expect("query check constraint");

    let mut literals = literals_in(&definition);
    literals.sort_unstable();

    let mut from_rust: Vec<&str> = cm_db::repo::users::AccountType::ALL
        .iter()
        .map(|kind| kind.as_str())
        .collect();
    from_rust.sort_unstable();

    assert_eq!(
        literals, from_rust,
        "constraint definition was: {definition}"
    );
}

/// An account is one side of the marketplace or the other, and the database
/// says so as well as the handlers: a homeowner cannot end up owning a listing
/// through a code path that forgets to check.
#[sqlx::test(migrations = "../../migrations")]
async fn a_homeowner_account_cannot_be_recorded_as_a_claimant(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    let homeowner = cm_core::new_id();
    cm_db::repo::users::insert(
        &mut conn,
        homeowner,
        "homeowner@example.test",
        "Homeowner",
        cm_db::repo::users::AccountType::Homeowner,
    )
    .await
    .expect("insert homeowner");

    let contractor_id = cm_core::new_id();
    sqlx::query(
        "INSERT INTO contractors (id, slug, display_name) \
         VALUES ($1, 'test-listing', 'Test Listing')",
    )
    .bind(contractor_id)
    .execute(&mut *conn)
    .await
    .expect("insert contractor");

    let refused = sqlx::query("UPDATE contractors SET claimed_by_user_id = $1 WHERE id = $2")
        .bind(homeowner)
        .bind(contractor_id)
        .execute(&mut *conn)
        .await;

    assert!(
        refused.is_err(),
        "the database must refuse a homeowner account as a claimant"
    );
}

/// The CHECK constraints actually reject bad rows, not merely exist.
#[sqlx::test(migrations = "../../migrations")]
async fn constraints_reject_invalid_rows(pool: PgPool) {
    let bad_kind = sqlx::query(
        "INSERT INTO regions (id, kind, code, name, centroid, source) \
         VALUES ($1, 'planet', '90042', 'Highland Park', \
                 ST_SetSRID(ST_MakePoint(0, 0), 4326)::geography, 'test')",
    )
    .bind(cm_core::new_id())
    .execute(&pool)
    .await;
    assert!(bad_kind.is_err(), "an unknown region kind must be rejected");

    let bad_slug = sqlx::query("INSERT INTO trades (id, slug, name) VALUES ($1, 'General Contractor', 'General Contractor')")
        .bind(cm_core::new_id())
        .execute(&pool)
        .await;
    assert!(bad_slug.is_err(), "a non-slug trade slug must be rejected");

    let ok = sqlx::query("INSERT INTO trades (id, slug, name, cslb_classification) VALUES ($1, 'general-contractor', 'General Contractor', 'B')")
        .bind(cm_core::new_id())
        .execute(&pool)
        .await;
    assert!(ok.is_ok(), "a valid trade must be accepted: {ok:?}");

    let duplicate_slug = sqlx::query(
        "INSERT INTO trades (id, slug, name) VALUES ($1, 'general-contractor', 'Duplicate')",
    )
    .bind(cm_core::new_id())
    .execute(&pool)
    .await;
    assert!(duplicate_slug.is_err(), "trade slugs must be unique");
}

async fn applied_rows(pool: &PgPool) -> Vec<(i64, i64)> {
    sqlx::query_as("SELECT version, execution_time FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("read migration ledger")
}

/// Pull the single-quoted literals out of a constraint definition.
fn literals_in(definition: &str) -> Vec<&str> {
    definition.split('\'').skip(1).step_by(2).collect()
}
