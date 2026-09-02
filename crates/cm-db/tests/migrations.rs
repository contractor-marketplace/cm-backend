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
    const EXEMPT: &[(&str, &str)] = &[
        ("audit_log", "updated_at"),
        // Append-only for the same reason: an event does not change, so the
        // column could only ever lie about when it last did.
        ("search_events", "updated_at"),
    ];

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

/// The outbox vocabularies in the database and in Rust must agree — the same
/// two-hand-written-lists drift the account-type check guards against.
#[sqlx::test(migrations = "../../migrations")]
async fn email_outbox_enums_match_the_rust_enums(pool: PgPool) {
    for (column, mut from_rust) in [
        (
            "kind",
            cm_db::repo::email_outbox::Kind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "status",
            cm_db::repo::email_outbox::MessageStatus::ALL
                .iter()
                .map(|status| status.as_str())
                .collect::<Vec<_>>(),
        ),
    ] {
        let definition: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(c.oid) \
             FROM pg_constraint c \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) \
             WHERE c.contype = 'c' AND c.conrelid = 'email_outbox'::regclass \
               AND a.attname = $1",
        )
        .bind(column)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("check constraint on {column}: {e}"));

        let mut literals = literals_in(&definition);
        literals.sort_unstable();
        from_rust.sort_unstable();

        assert_eq!(
            literals, from_rust,
            "{column}: constraint definition was: {definition}"
        );
    }
}

/// A saved search's facet vocabularies must be the job table's, verbatim: the
/// reverse match compares these columns to `jobs.timeline` and
/// `jobs.build_type` directly, so a value allowed on one side and not the
/// other is a search that can never fire.
#[sqlx::test(migrations = "../../migrations")]
async fn saved_search_enums_match_the_job_columns(pool: PgPool) {
    for (column, mut from_rust) in [
        (
            "timeline",
            cm_db::repo::jobs::JobTimeline::ALL
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "build_type",
            cm_db::repo::jobs::BuildType::ALL
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>(),
        ),
    ] {
        let definition: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(c.oid) \
             FROM pg_constraint c \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) \
             WHERE c.contype = 'c' AND c.conrelid = 'saved_searches'::regclass \
               AND a.attname = $1",
        )
        .bind(column)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("check constraint on {column}: {e}"));

        let mut literals = literals_in(&definition);
        literals.sort_unstable();
        from_rust.sort_unstable();

        assert_eq!(
            literals, from_rust,
            "{column}: constraint definition was: {definition}"
        );
    }
}

/// An owner-supplied address is all four parts or none.
///
/// This is what makes the per-column COALESCE in `geocodable_address` safe. If
/// a half-filled address were representable, that query could take the owner's
/// street and the licence's city and geocode a building that exists nowhere.
#[sqlx::test(migrations = "../../migrations")]
async fn a_partial_owner_address_is_refused(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    let id = cm_core::new_id();
    sqlx::query(
        "INSERT INTO contractors (id, slug, display_name) VALUES ($1, 'partial-address', 'X')",
    )
    .bind(id)
    .execute(&mut *conn)
    .await
    .expect("insert contractor");

    // Every proper subset of the four columns must be rejected; the whole set
    // must be accepted.
    for (label, sql) in [
        ("street only", "owner_address_line1 = '1 Main St'"),
        (
            "street and city",
            "owner_address_line1 = '1 Main St', owner_address_city = 'Burbank'",
        ),
        (
            "all but the ZIP",
            "owner_address_line1 = '1 Main St', owner_address_city = 'Burbank', \
             owner_address_state = 'CA'",
        ),
    ] {
        let result = sqlx::query(&format!("UPDATE contractors SET {sql} WHERE id = $1"))
            .bind(id)
            .execute(&mut *conn)
            .await;
        assert!(result.is_err(), "a partial address ({label}) was accepted");
    }

    sqlx::query(
        "UPDATE contractors SET owner_address_line1 = '1 Main St', \
             owner_address_city = 'Burbank', owner_address_state = 'CA', \
             owner_address_postal_code = '91504' WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *conn)
    .await
    .expect("a whole address must be accepted");
}

/// The pin follows the address the page shows.
///
/// A contractor who corrects their address and keeps the old pin is the same
/// bug as search and map disagreeing, arriving from a new direction.
#[sqlx::test(migrations = "../../migrations")]
async fn the_geocodable_address_prefers_the_owners(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    // A licence record needs the import run it arrived in — the FK is the
    // provenance rule that every published address is traceable to a file.
    let run_id = cm_core::new_id();
    sqlx::query(
        // Status left at its 'running' default: `license_import_runs_finished_iff_done`
        // requires a finished run to carry a `finished_at`, and this fixture
        // only needs the row to exist for the foreign key.
        "INSERT INTO license_import_runs \
             (id, source, source_file_name, source_file_sha256) \
         VALUES ($1, 'cslb_master_list', 'fixture.csv', $2)",
    )
    .bind(run_id)
    .bind(vec![1u8; 32])
    .execute(&mut *conn)
    .await
    .expect("insert import run");

    let license_id = cm_core::new_id();
    sqlx::query(
        "INSERT INTO license_records \
             (id, license_no, business_name, status, status_raw, address_line1, city, state, \
              postal_code, raw, content_hash, first_run_id, last_run_id) \
         VALUES ($1, '999999', 'X', 'active', 'ACTIVE', '456 Old Ave', 'Los Angeles', 'CA', \
                 '90042', '{}'::jsonb, $3, $2, $2)",
    )
    .bind(license_id)
    .bind(run_id)
    // A real SHA-256 width: the table constrains this to 32 bytes, which is
    // the check that stops a truncated or placeholder digest being stored.
    .bind(vec![0u8; 32])
    .execute(&mut *conn)
    .await
    .expect("insert licence");

    let id = cm_core::new_id();
    sqlx::query(
        "INSERT INTO contractors (id, slug, display_name, license_record_id) \
         VALUES ($1, 'pin-follows', 'X', $2)",
    )
    .bind(id)
    .bind(license_id)
    .execute(&mut *conn)
    .await
    .expect("insert contractor");

    let before = cm_db::repo::contractors::geocodable_address(&mut conn, id)
        .await
        .expect("address");
    assert_eq!(
        before.as_deref(),
        Some("456 Old Ave, Los Angeles, CA, 90042"),
        "with no owner address it must resolve the licence's"
    );

    sqlx::query(
        "UPDATE contractors SET owner_address_line1 = '123 Real St', \
             owner_address_city = 'Burbank', owner_address_state = 'CA', \
             owner_address_postal_code = '91504' WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *conn)
    .await
    .expect("set owner address");

    let after = cm_db::repo::contractors::geocodable_address(&mut conn, id)
        .await
        .expect("address");
    assert_eq!(
        after.as_deref(),
        Some("123 Real St, Burbank, CA, 91504"),
        "the owner's address must win, whole, with nothing borrowed from the licence"
    );
}

/// A photo is a key and its dimensions together, or nothing at all.
#[sqlx::test(migrations = "../../migrations")]
async fn a_half_recorded_photo_is_refused(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    let id = cm_core::new_id();
    sqlx::query("INSERT INTO contractors (id, slug, display_name) VALUES ($1, 'half-photo', 'X')")
        .bind(id)
        .execute(&mut *conn)
        .await
        .expect("insert contractor");

    for (label, sql) in [
        (
            "a key with no dimensions",
            "photo_storage_key = 'contractors/a/b.jpg'",
        ),
        (
            "dimensions with no key",
            "photo_width = 100, photo_height = 100",
        ),
    ] {
        let result = sqlx::query(&format!("UPDATE contractors SET {sql} WHERE id = $1"))
            .bind(id)
            .execute(&mut *conn)
            .await;
        assert!(result.is_err(), "{label} was accepted");
    }
}

/// The review-source vocabulary in the database and in Rust must agree, for the
/// same reason account types must. One variant today; the pairing exists so
/// that adding a second one to either list without the other fails here rather
/// than as an unparseable row at read time.
#[sqlx::test(migrations = "../../migrations")]
async fn review_source_matches_the_rust_enum(pool: PgPool) {
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) \
         FROM pg_constraint c \
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) \
         WHERE c.contype = 'c' AND c.conrelid = 'contractor_reviews'::regclass \
           AND a.attname = 'source'",
    )
    .fetch_one(&pool)
    .await
    .expect("query check constraint");

    let mut literals = literals_in(&definition);
    literals.sort_unstable();

    let mut from_rust: Vec<&str> = cm_db::repo::reviews::ReviewSource::ALL
        .iter()
        .map(|source| source.as_str())
        .collect();
    from_rust.sort_unstable();

    assert_eq!(
        literals, from_rust,
        "constraint definition was: {definition}"
    );
}

/// A contractor's reviews go when the contractor does.
///
/// The FK is ON DELETE CASCADE, which is the right call for rows that are
/// meaningless without their parent — but a cascade is easy to write and easy
/// to get wrong, and orphaned reviews would be invisible until someone counted.
#[sqlx::test(migrations = "../../migrations")]
async fn deleting_a_contractor_takes_its_reviews_with_it(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    let contractor_id = cm_core::new_id();
    sqlx::query(
        "INSERT INTO contractors (id, slug, display_name) \
         VALUES ($1, 'review-cascade', 'Review Cascade')",
    )
    .bind(contractor_id)
    .execute(&mut *conn)
    .await
    .expect("insert contractor");

    sqlx::query(
        "INSERT INTO contractor_reviews \
             (id, contractor_id, source, external_id, rating, position) \
         VALUES ($1, $2, 'google', 'ext-1', 5.0, 1)",
    )
    .bind(cm_core::new_id())
    .bind(contractor_id)
    .execute(&mut *conn)
    .await
    .expect("insert review");

    sqlx::query("DELETE FROM contractors WHERE id = $1")
        .bind(contractor_id)
        .execute(&mut *conn)
        .await
        .expect("delete contractor");

    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM contractor_reviews WHERE contractor_id = $1")
            .bind(contractor_id)
            .fetch_one(&mut *conn)
            .await
            .expect("count reviews");

    assert_eq!(left, 0, "reviews outlived the contractor they belong to");
}

/// A rating outside 1–5 is not a rating, and the database is the last place
/// that can say so before it renders as a row of broken stars.
#[sqlx::test(migrations = "../../migrations")]
async fn a_review_rating_must_be_between_one_and_five(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    let contractor_id = cm_core::new_id();
    sqlx::query(
        "INSERT INTO contractors (id, slug, display_name) \
         VALUES ($1, 'review-range', 'Review Range')",
    )
    .bind(contractor_id)
    .execute(&mut *conn)
    .await
    .expect("insert contractor");

    for bad in ["0.0", "5.5"] {
        let result = sqlx::query(&format!(
            "INSERT INTO contractor_reviews \
                 (id, contractor_id, source, external_id, rating, position) \
             VALUES ($1, $2, 'google', 'ext-{bad}', {bad}, 1)"
        ))
        .bind(cm_core::new_id())
        .bind(contractor_id)
        .execute(&mut *conn)
        .await;

        assert!(result.is_err(), "a rating of {bad} was accepted");
    }
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
        Some("homeowner@example.test"),
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

/// Job status and timeline vocabularies must match their Rust enums, for the
/// same reason account_type does: two hand-written lists that drift apart
/// surface as a 500 rather than a compile error.
///
/// Restricted to single-column CHECKs. `jobs.status` appears in two of them —
/// its own vocabulary, and `jobs_closed_iff_not_open`, which spans status and
/// closed_at — and without the restriction this test picks whichever the
/// catalogue returns first.
#[sqlx::test(migrations = "../../migrations")]
async fn job_vocabularies_match_the_rust_enums(pool: PgPool) {
    for (column, from_rust) in [
        (
            "status",
            cm_db::repo::jobs::JobStatus::ALL
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "timeline",
            cm_db::repo::jobs::JobTimeline::ALL
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "build_type",
            cm_db::repo::jobs::BuildType::ALL
                .iter()
                .map(|b| b.as_str())
                .collect::<Vec<_>>(),
        ),
    ] {
        let definition: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(c.oid) \
             FROM pg_constraint c \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) \
             WHERE c.contype = 'c' AND c.conrelid = 'jobs'::regclass AND a.attname = $1 \
               AND array_length(c.conkey, 1) = 1",
        )
        .bind(column)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|_| panic!("no CHECK constraint on jobs.{column}"));

        let mut literals = literals_in(&definition);
        literals.sort_unstable();
        let mut expected = from_rust;
        expected.sort_unstable();

        assert_eq!(literals, expected, "jobs.{column} was: {definition}");
    }
}

/// Posting work is the homeowner's side, and the database says so as well as
/// the handler — a code path that skips the check cannot record a contractor
/// account as the poster.
#[sqlx::test(migrations = "../../migrations")]
async fn a_contractor_account_cannot_be_recorded_as_a_job_poster(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    let contractor = cm_core::new_id();
    cm_db::repo::users::insert(
        &mut conn,
        contractor,
        Some("contractor@example.test"),
        "Contractor",
        cm_db::repo::users::AccountType::Contractor,
    )
    .await
    .expect("insert contractor");

    let refused = sqlx::query(
        "INSERT INTO jobs (id, posted_by_user_id, title, description) \
         VALUES ($1, $2, 'A job', 'Some detail')",
    )
    .bind(cm_core::new_id())
    .bind(contractor)
    .execute(&mut *conn)
    .await;

    assert!(
        refused.is_err(),
        "the database must refuse a contractor account as a job poster"
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

/// The default travel radius is written in three places that cannot disagree:
/// the column default in 0030, that migration's partial index, and
/// `DEFAULT_SERVICE_RADIUS_M` in Rust.
///
/// The column half of this is loud when it breaks — everyone gets the wrong
/// coverage. The index half is silent, which is why it is pinned here: a
/// partial index whose predicate no longer matches the query's is simply not
/// used, so the only symptom is a search that got slower, and only under enough
/// rows to notice.
#[sqlx::test(migrations = "../../migrations")]
async fn the_default_service_radius_is_the_same_number_everywhere(pool: PgPool) {
    let column_default: String = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
          WHERE table_name = 'contractors' AND column_name = 'service_radius_m'",
    )
    .fetch_one(&pool)
    .await
    .expect("read the column default");

    let expected = cm_db::repo::search::DEFAULT_SERVICE_RADIUS_M.to_string();
    assert!(
        column_default.contains(&expected),
        "the column defaults to {column_default}, Rust says {expected}"
    );

    let index: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'contractors_custom_radius_gix'",
    )
    .fetch_one(&pool)
    .await
    .expect("the partial index exists");

    assert!(
        index.contains(&expected),
        "the index selects on a different number than Rust queries with. \
         It will silently stop being used. index: {index}, Rust: {expected}"
    );
    assert!(
        index.contains("service_radius_m <>"),
        "the index must cover exactly the contractors who overrode the \
         default, since those are the only ones the pre-query looks for: {index}"
    );
}

/// Every listing covers somewhere, including the unclaimed majority.
///
/// This is the property the whole coverage model rests on: search asks "who
/// travels to this address", and before 0030 a listing that had declared
/// nothing answered "nowhere" — which was every listing, because service areas
/// are set by claimants and almost nothing is claimed.
#[sqlx::test(migrations = "../../migrations")]
async fn a_contractor_covers_somewhere_without_anybody_saying_so(pool: PgPool) {
    let radius: i32 = sqlx::query_scalar(
        "INSERT INTO contractors (id, slug, display_name) \
         VALUES ($1, 'default-radius-co', 'Default Radius Co') \
         RETURNING service_radius_m",
    )
    .bind(cm_core::new_id())
    .fetch_one(&pool)
    .await
    .expect("insert a contractor without mentioning a radius");

    assert_eq!(
        radius,
        cm_db::repo::search::DEFAULT_SERVICE_RADIUS_M,
        "a contractor nobody has claimed still has to cover its own city"
    );

    // The ceiling is enforced by the database, not only by the repo that
    // clamps: an area of a thousand miles is not a service area.
    let absurd = sqlx::query("UPDATE contractors SET service_radius_m = 2000000 WHERE slug = $1")
        .bind("default-radius-co")
        .execute(&pool)
        .await;
    assert!(
        absurd.is_err(),
        "the radius ceiling must hold in the schema"
    );
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
