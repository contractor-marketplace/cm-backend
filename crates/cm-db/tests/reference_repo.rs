//! Reference data: the ZIP-code centroids every unlocated listing falls back to.

use cm_db::repo::reference;
use cm_db::PgPool;

/// The Census gazetteer is the only complete source of ZIP centroids and it
/// publishes no names, so a bulk load carries the code in the name column. That
/// placeholder must not overwrite a name somebody curated — otherwise loading
/// the file turns "Silver Lake" into "90026" for every ZIP that had one, and
/// the only way back is to notice and re-curate.
#[sqlx::test(migrations = "../../migrations")]
async fn a_placeholder_name_never_overwrites_a_real_one(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    reference::upsert_zcta(
        &mut conn,
        "90026",
        "Silver Lake",
        34.0781,
        -118.2606,
        "curated",
    )
    .await
    .expect("insert");

    // A gazetteer row: authoritative centroid, no name.
    reference::upsert_zcta(
        &mut conn,
        "90026",
        "90026",
        34.080017,
        -118.262643,
        "census",
    )
    .await
    .expect("bulk load");

    let region = reference::find_zcta(&mut conn, "90026")
        .await
        .expect("query")
        .expect("present");

    assert_eq!(region.name, "Silver Lake", "the curated name must survive");
    assert!(
        (region.lat - 34.080_017).abs() < 1e-6,
        "the centroid is the file's to update: {}",
        region.lat
    );
}

/// A ZIP nobody has named is stored under its own code. The column is NOT NULL
/// and rejects blanks, so there has to be *something*, and the code is the one
/// value that is always true.
#[sqlx::test(migrations = "../../migrations")]
async fn an_unnamed_zip_is_stored_under_its_code(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    reference::upsert_zcta(&mut conn, "90001", "90001", 33.974026, -118.24951, "census")
        .await
        .expect("insert");

    let region = reference::find_zcta(&mut conn, "90001")
        .await
        .expect("query")
        .expect("present");

    assert_eq!(region.name, "90001");
}

/// A real name still replaces an earlier one — the rule is about placeholders,
/// not about making the column immutable.
#[sqlx::test(migrations = "../../migrations")]
async fn a_real_name_still_updates(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");

    reference::upsert_zcta(&mut conn, "90042", "90042", 34.1156, -118.1926, "census")
        .await
        .expect("insert");
    reference::upsert_zcta(
        &mut conn,
        "90042",
        "Highland Park",
        34.1156,
        -118.1926,
        "curated",
    )
    .await
    .expect("name it");

    let region = reference::find_zcta(&mut conn, "90042")
        .await
        .expect("query")
        .expect("present");

    assert_eq!(region.name, "Highland Park");
}

/// The shipped centroid file has to load, and has to cover the county the
/// product serves. A file that parses but omits most of Los Angeles would pass
/// every other test in this suite and leave the map empty.
#[sqlx::test(migrations = "../../migrations")]
async fn the_shipped_centroid_file_covers_the_county(pool: PgPool) {
    #[derive(serde::Deserialize)]
    struct Row {
        code: String,
        name: String,
        lat: f64,
        lon: f64,
    }

    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/data/zcta_ca.csv");
    let mut reader = csv::Reader::from_path(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));

    let mut conn = pool.acquire().await.expect("connection");
    let mut loaded = 0;
    for row in reader.deserialize::<Row>() {
        let row = row.expect("well-formed row");
        assert!(
            (-90.0..=90.0).contains(&row.lat) && (-180.0..=180.0).contains(&row.lon),
            "{} has an impossible centroid",
            row.code
        );
        reference::upsert_zcta(&mut conn, &row.code, &row.name, row.lat, row.lon, "test")
            .await
            .expect("upsert");
        loaded += 1;
    }

    assert!(loaded > 1_500, "expected the California set, got {loaded}");

    // A spread of Los Angeles County ZIPs, including several that the
    // 25-row file this replaced did not carry.
    for code in [
        "90001", "90026", "90042", "90210", "90638", "91340", "93536",
    ] {
        assert!(
            reference::find_zcta(&mut conn, code)
                .await
                .expect("query")
                .is_some(),
            "{code} is missing from the shipped centroid file"
        );
    }
}
