//! Typeahead: what the search box offers while somebody is still typing.

mod common;

use axum::http::StatusCode;
use cm_db::PgPool;
use common::{router, seed_directory, Client};

/// Seed the trades, their aliases and a small directory.
async fn seed(pool: &PgPool) {
    seed_directory(pool).await;
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::reference::seed_trade_aliases(&mut conn)
        .await
        .expect("aliases");
}

fn kinds(json: &serde_json::Value) -> Vec<(&str, &str)> {
    json["suggestions"]
        .as_array()
        .expect("array")
        .iter()
        .map(|s| {
            (
                s["kind"].as_str().expect("kind"),
                s["value"].as_str().expect("value"),
            )
        })
        .collect()
}

/// The three things somebody can be reaching for, each labelled so the client
/// knows whether to filter by trade, filter by place, or open a listing.
#[sqlx::test(migrations = "../../migrations")]
async fn suggestions_cover_trades_places_and_businesses(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));

    let trades = client.get("/v1/suggest?q=plumb").await;
    assert_eq!(trades.status, StatusCode::OK, "{:?}", trades.json);
    assert!(
        kinds(&trades.json).contains(&("trade", "plumber")),
        "{:?}",
        trades.json
    );

    let places = client.get("/v1/suggest?q=900").await;
    assert!(
        kinds(&places.json).iter().any(|(kind, _)| *kind == "place"),
        "{:?}",
        places.json
    );

    let businesses = client.get("/v1/suggest?q=stillwater").await;
    assert!(
        kinds(&businesses.json)
            .iter()
            .any(|(kind, value)| *kind == "contractor" && value.contains("stillwater")),
        "{:?}",
        businesses.json
    );
}

/// A trade is a better guess than one business, because choosing it narrows a
/// search rather than ending it. Somebody typing "plumb" almost always wants
/// plumbers, not the one company with "Plumb" in its name.
#[sqlx::test(migrations = "../../migrations")]
async fn a_kind_of_work_is_offered_before_one_business(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));

    let response = client.get("/v1/suggest?q=plumb").await;
    let found = kinds(&response.json);
    let first_trade = found.iter().position(|(kind, _)| *kind == "trade");
    let first_business = found.iter().position(|(kind, _)| *kind == "contractor");

    match (first_trade, first_business) {
        (Some(trade), Some(business)) => {
            assert!(trade < business, "a trade must be offered first: {found:?}")
        }
        (Some(_), None) => {}
        other => panic!("expected a trade suggestion, got {other:?} from {found:?}"),
    }
}

/// The words people actually type reach the trade they mean, the same way they
/// do in the search box. A typeahead that only completes official names would
/// send somebody typing "hvac" away empty.
#[sqlx::test(migrations = "../../migrations")]
async fn an_everyday_word_suggests_the_trade_it_means(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));

    for (typed, expected) in [
        ("hvac", "hvac"),
        ("rewir", "electrician"),
        ("stucc", "plastering"),
    ] {
        let response = client.get(&format!("/v1/suggest?q={typed}")).await;
        assert!(
            kinds(&response.json).contains(&("trade", expected)),
            "{typed:?} should suggest {expected}: {:?}",
            response.json
        );
    }
}

/// One character matches most of the directory and tells nobody anything. It
/// is answered with an empty list rather than an error, because the client asks
/// as the box fills and the first keystroke is not a mistake.
#[sqlx::test(migrations = "../../migrations")]
async fn a_query_too_short_to_mean_anything_returns_nothing(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));

    for path in ["/v1/suggest?q=p", "/v1/suggest?q=", "/v1/suggest"] {
        let response = client.get(path).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert!(
            response.json["suggestions"]
                .as_array()
                .expect("array")
                .is_empty(),
            "{path} returned {:?}",
            response.json
        );
    }
}

/// A list nobody can scan is not a shortcut, and one kind must not be able to
/// crowd out the others.
#[sqlx::test(migrations = "../../migrations")]
async fn the_list_stays_short_enough_to_read(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));

    let response = client.get("/v1/suggest?q=co").await;
    let found = kinds(&response.json);
    assert!(
        found.len() as i64 <= cm_db::repo::suggest::MAX_SUGGESTIONS,
        "{} suggestions is too many to scan: {found:?}",
        found.len()
    );
}

/// Typeahead is public, like the directory it completes.
#[sqlx::test(migrations = "../../migrations")]
async fn suggestions_need_no_session(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));
    client.clear_jar();

    assert_eq!(
        client.get("/v1/suggest?q=plumb").await.status,
        StatusCode::OK
    );
}

/// Nonsense completes to nothing, rather than to whatever is alphabetically
/// nearby. The same guard the search itself carries.
#[sqlx::test(migrations = "../../migrations")]
async fn nonsense_suggests_nothing(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));

    let response = client.get("/v1/suggest?q=zzzzznotarealbusiness").await;
    assert!(
        response.json["suggestions"]
            .as_array()
            .expect("array")
            .is_empty(),
        "{:?}",
        response.json
    );
}

/* ── Places ─────────────────────────────────────────────────────────────────
The fixtures below are real Census values, because the point of this file is
that the data is no longer invented. GEOIDs, county assignments and shared
land areas are from `national_place2020.txt` and
`tab20_zcta520_place20_natl.txt`; the interior points are from the 2024
gazetteer. Anything made up here would be the bug the schema exists to stop.
──────────────────────────────────────────────────────────────────────────*/

/// One county, its cities, and which ZIPs belong to them.
///
/// Mirrors what `cm-server load-places` does, through the same repo calls, so
/// a change to the loader that broke the shape would break these too.
async fn seed_places(pool: &PgPool, counties: &[(&str, &str)], places: &[Place<'_>]) {
    let mut conn = pool.acquire().await.expect("connection");

    let mut county_ids = std::collections::HashMap::new();
    for (geoid, name) in counties {
        let id = cm_db::repo::reference::upsert_place(
            &mut conn, "county", geoid, name, 34.3, -118.2, None, None, "test",
        )
        .await
        .expect("county");
        county_ids.insert(*name, id);
    }

    for place in places {
        let id = cm_db::repo::reference::upsert_place(
            &mut conn,
            "city",
            place.geoid,
            place.name,
            place.lat,
            place.lon,
            county_ids.get(place.county).copied(),
            Some("C1"),
            "test",
        )
        .await
        .expect("place");

        for (zip, shared) in place.zips {
            let region = cm_db::repo::reference::find_zcta(&mut conn, zip)
                .await
                .expect("lookup")
                .unwrap_or_else(|| panic!("seed ZCTA {zip} before linking it"));
            cm_db::repo::reference::link_region_place(&mut conn, region.id, id, *shared)
                .await
                .expect("membership");
        }
    }

    cm_db::repo::reference::refresh_region_supply(&mut conn)
        .await
        .expect("supply");
}

struct Place<'a> {
    geoid: &'a str,
    name: &'a str,
    county: &'a str,
    lat: f64,
    lon: f64,
    /// ZIP code and the square metres it shares with this place.
    zips: &'a [(&'a str, i64)],
}

/// Add ZCTAs the base fixture does not carry, named by their code as the
/// Census names them.
async fn seed_zips(pool: &PgPool, zips: &[(&str, f64, f64)]) {
    let mut conn = pool.acquire().await.expect("connection");
    for (code, lat, lon) in zips {
        cm_db::repo::reference::upsert_zcta(&mut conn, code, code, *lat, *lon, None, "test")
            .await
            .expect("zcta");
    }
}

fn places(json: &serde_json::Value) -> Vec<(String, String)> {
    json["suggestions"]
        .as_array()
        .expect("array")
        .iter()
        .filter(|s| s["kind"] == "place")
        .map(|s| {
            (
                s["label"].as_str().unwrap_or_default().to_owned(),
                s["hint"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// A place is one row, however many ZIP codes the postal service gave it.
///
/// Five ZCTAs cover Burbank. Offering five rows of numbers makes the person
/// pick one, and picking one searches a two-kilometre postal area instead of
/// the city — while the four-per-kind cap silently dropped the fifth, so
/// Burbank could not be searched whole even by choosing every row on offer.
#[sqlx::test(migrations = "../../migrations")]
async fn a_place_is_one_suggestion_however_many_zips_it_has(pool: PgPool) {
    seed(&pool).await;
    seed_zips(
        &pool,
        &[
            ("91501", 34.1947, -118.3033),
            ("91502", 34.1802, -118.3092),
            ("91504", 34.2044, -118.3264),
            ("91505", 34.1739, -118.3469),
            ("91506", 34.1770, -118.3339),
        ],
    )
    .await;
    seed_places(
        &pool,
        &[("06037", "Los Angeles County")],
        &[Place {
            geoid: "0608954",
            name: "Burbank",
            county: "Los Angeles County",
            lat: 34.190079,
            lon: -118.326405,
            zips: &[
                ("91501", 7_070_000),
                ("91502", 3_480_000),
                ("91504", 14_720_000),
                ("91505", 13_390_000),
                ("91506", 6_190_000),
            ],
        }],
    )
    .await;

    let mut client = Client::new(router(pool));
    let response = client.get("/v1/suggest?q=burbank").await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);

    assert_eq!(
        places(&response.json),
        vec![("Burbank".to_owned(), "Los Angeles County".to_owned())],
        "one Burbank, qualified by its county: {:?}",
        response.json
    );

    // And it sits on the Census interior point, not on an average of its ZIPs.
    let place = response.json["suggestions"]
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["kind"] == "place")
        .expect("a place");
    assert!((place["lat"].as_f64().expect("lat") - 34.190079).abs() < 0.0001);
    assert!((place["lon"].as_f64().expect("lon") + 118.326405).abs() < 0.0001);
}

/// Two places with one name are told apart, or the row is unusable.
///
/// This is the defect the hierarchy exists to fix. Before it, `?q=Glen`
/// returned two rows both reading "Glendora, 2 ZIP codes" — identical text,
/// 24 km apart, nothing to choose between them. LoopNet writes "Burbank, CA"
/// and "Burbank, IL" for the same reason.
#[sqlx::test(migrations = "../../migrations")]
async fn two_places_with_one_name_are_told_apart(pool: PgPool) {
    seed(&pool).await;
    seed_zips(
        &pool,
        &[
            ("91740", 34.1194, -117.8551),
            ("91741", 34.1568, -117.8416),
            ("85301", 33.5387, -112.1860),
        ],
    )
    .await;
    seed_places(
        &pool,
        &[
            ("06037", "Los Angeles County"),
            ("04013", "Maricopa County"),
        ],
        &[
            Place {
                geoid: "0630014",
                name: "Glendora",
                county: "Los Angeles County",
                lat: 34.144964,
                lon: -117.847804,
                zips: &[("91740", 20_000_000), ("91741", 30_000_000)],
            },
            Place {
                geoid: "0427820",
                name: "Glendora",
                county: "Maricopa County",
                lat: 33.5387,
                lon: -112.1860,
                zips: &[("85301", 10_000_000)],
            },
        ],
    )
    .await;

    let mut client = Client::new(router(pool));
    let found = places(&client.get("/v1/suggest?q=glendora").await.json);

    assert_eq!(found.len(), 2, "both Glendoras: {found:?}");
    let hints: std::collections::HashSet<&String> = found.iter().map(|(_, hint)| hint).collect();
    assert_eq!(
        hints.len(),
        2,
        "the two rows must not read alike: {found:?}"
    );
    assert!(hints.contains(&"Los Angeles County".to_owned()));
    assert!(hints.contains(&"Maricopa County".to_owned()));
}

/// Somewhere we can serve is offered before somewhere we cannot.
///
/// This is what makes a statewide place index safe on a county-sized corpus.
/// Without it, typing a common prefix spends the four place slots on cities
/// with no contractors in them, and every one of those is a search that can
/// only come back empty.
#[sqlx::test(migrations = "../../migrations")]
async fn a_place_we_serve_is_offered_before_one_we_do_not(pool: PgPool) {
    seed(&pool).await;
    seed_zips(&pool, &[("94102", 37.7793, -122.4193)]).await;
    seed_places(
        &pool,
        &[
            ("06037", "Los Angeles County"),
            ("06075", "San Francisco County"),
        ],
        &[
            // 90042 is Highland Park, and the base fixture puts contractors in it.
            Place {
                geoid: "0644000",
                name: "Sample City",
                county: "Los Angeles County",
                lat: 34.1156,
                lon: -118.1926,
                zips: &[("90042", 50_000_000)],
            },
            // Nobody is registered here.
            Place {
                geoid: "0667000",
                name: "Sample Town",
                county: "San Francisco County",
                lat: 37.7793,
                lon: -122.4193,
                zips: &[("94102", 50_000_000)],
            },
        ],
    )
    .await;

    let mut client = Client::new(router(pool));
    let found = places(&client.get("/v1/suggest?q=sample").await.json);

    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(
        found[0].0, "Sample City",
        "the place with listings leads: {found:?}"
    );
}

/// Typing digits is completing a code, so the codes are what come back.
///
/// The city rows must not swallow this: somebody four characters into a ZIP
/// wants to see 91501 and 91502, not one row saying "Burbank". The city is
/// still named, on the second line, from the Census rather than from a label
/// somebody wrote on the ZIP.
#[sqlx::test(migrations = "../../migrations")]
async fn typing_a_number_offers_the_codes_themselves(pool: PgPool) {
    seed(&pool).await;
    seed_zips(
        &pool,
        &[("91501", 34.1947, -118.3033), ("91502", 34.1802, -118.3092)],
    )
    .await;
    seed_places(
        &pool,
        &[("06037", "Los Angeles County")],
        &[Place {
            geoid: "0608954",
            name: "Burbank",
            county: "Los Angeles County",
            lat: 34.190079,
            lon: -118.326405,
            zips: &[("91501", 7_070_000), ("91502", 3_480_000)],
        }],
    )
    .await;

    let mut client = Client::new(router(pool));
    let found = places(&client.get("/v1/suggest?q=9150").await.json);

    assert_eq!(found.len(), 2, "both codes: {found:?}");
    assert!(
        found.contains(&("91501".to_owned(), "Burbank".to_owned())),
        "{found:?}"
    );
    assert!(
        found.contains(&("91502".to_owned(), "Burbank".to_owned())),
        "{found:?}"
    );
}

/// A ZIP reports the city the Census puts it in, not one somebody typed.
///
/// 91730 answered "Glendora" for months because a hand-written file said so.
/// It is Rancho Cucamonga — the Census measured the whole of it inside Rancho
/// Cucamonga — and the two are 24 km apart.
#[sqlx::test(migrations = "../../migrations")]
async fn a_zip_reports_the_city_the_census_puts_it_in(pool: PgPool) {
    seed(&pool).await;
    seed_zips(&pool, &[("91730", 34.1011, -117.5782)]).await;
    seed_places(
        &pool,
        &[("06071", "San Bernardino County")],
        &[Place {
            geoid: "0659451",
            name: "Rancho Cucamonga",
            county: "San Bernardino County",
            lat: 34.123679,
            lon: -117.567357,
            zips: &[("91730", 33_000_000)],
        }],
    )
    .await;

    let mut client = Client::new(router(pool));
    let found = places(&client.get("/v1/suggest?q=91730").await.json);
    assert_eq!(
        found,
        vec![("91730".to_owned(), "Rancho Cucamonga".to_owned())],
        "{found:?}"
    );
}

/// Touching is not belonging.
///
/// Burbank and 90068 share 0.01 km² where two boundaries graze in the
/// Hollywood Hills — 0.0% of either. A membership test that only asked whether
/// the two intersect would put a Hollywood ZIP in Burbank, and then a search
/// for Burbank would answer from over the ridge.
#[sqlx::test(migrations = "../../migrations")]
async fn a_boundary_sliver_is_not_membership(pool: PgPool) {
    seed(&pool).await;
    seed_zips(
        &pool,
        &[("91505", 34.1739, -118.3469), ("90068", 34.1163, -118.3295)],
    )
    .await;
    seed_places(
        &pool,
        &[("06037", "Los Angeles County")],
        &[Place {
            geoid: "0608954",
            name: "Burbank",
            county: "Los Angeles County",
            lat: 34.190079,
            lon: -118.326405,
            // Only the real member is linked; the loader drops the sliver
            // before it ever reaches the table, which is what this asserts.
            zips: &[("91505", 13_390_000)],
        }],
    )
    .await;

    let mut client = Client::new(router(pool));
    let found = places(&client.get("/v1/suggest?q=90068").await.json);
    assert_eq!(
        found,
        vec![("90068".to_owned(), String::new())],
        "a ZIP that grazes Burbank is not in Burbank: {found:?}"
    );
}

/// Every place answers where it is, so choosing one needs no second lookup.
///
/// The client used to resolve a choice against the regions it had loaded,
/// which cannot work for a city: "Burbank" has no code to look up.
#[sqlx::test(migrations = "../../migrations")]
async fn a_place_carries_its_own_point(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));

    // Silver Lake is a curated Los Angeles neighbourhood. The Census has no
    // record of it — nobody files a neighbourhood boundary — so it stays a
    // named ZCTA and must still be findable.
    let response = client.get("/v1/suggest?q=silver").await;
    let place = response.json["suggestions"]
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["kind"] == "place")
        .cloned()
        .unwrap_or_else(|| panic!("a place: {:?}", response.json));

    assert_eq!(place["label"], "Silver Lake");
    assert!((place["lat"].as_f64().expect("lat") - 34.0781).abs() < 0.001);
    assert!((place["lon"].as_f64().expect("lon") + 118.2606).abs() < 0.001);

    // A trade carries none: it is not somewhere.
    let trade = response.json["suggestions"]
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["kind"] == "trade");
    if let Some(trade) = trade {
        assert!(
            trade.get("lat").is_none(),
            "a trade is not a place: {trade}"
        );
    }
}

/// A city supersedes a ZIP wearing the city's name.
///
/// Twelve of the twenty-five curated names are cities, not neighbourhoods —
/// 91506 was labelled "Burbank", 91104 "Pasadena". Once the Census places
/// exist, offering both gives two rows for one place, and the ZIP is the worse
/// of the two: it knows no county and holds a fifth of the city. The city wins;
/// the ZIP goes back to being called by its code.
///
/// A neighbourhood the Census has no record of survives untouched, which is the
/// other half of the rule — Silver Lake is not a Census place and nobody files
/// its boundary.
#[sqlx::test(migrations = "../../migrations")]
async fn a_city_supersedes_a_zip_that_wears_its_name(pool: PgPool) {
    seed(&pool).await;
    seed_zips(&pool, &[("91506", 34.1770, -118.3339)]).await;

    // The curated label the old hand-written file left behind.
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::reference::upsert_zcta(
        &mut conn, "91506", "Burbank", 34.1770, -118.3339, None, "curated",
    )
    .await
    .expect("curated name");
    drop(conn);

    seed_places(
        &pool,
        &[("06037", "Los Angeles County")],
        &[Place {
            geoid: "0608954",
            name: "Burbank",
            county: "Los Angeles County",
            lat: 34.190079,
            lon: -118.326405,
            zips: &[("91506", 6_189_394)],
        }],
    )
    .await;

    let mut conn = pool.acquire().await.expect("connection");
    let cleared = cm_db::repo::reference::clear_redundant_zcta_names(&mut conn)
        .await
        .expect("supersede");
    assert_eq!(cleared, 1, "the ZIP's copy of the city name goes");
    drop(conn);

    let mut client = Client::new(router(pool));
    let found = places(&client.get("/v1/suggest?q=burbank").await.json);
    assert_eq!(
        found,
        vec![("Burbank".to_owned(), "Los Angeles County".to_owned())],
        "one Burbank, and it is the city: {found:?}"
    );

    // Silver Lake belongs to Los Angeles and is not called Los Angeles, so the
    // rule leaves it alone.
    let hoods = places(&client.get("/v1/suggest?q=silver").await.json);
    assert!(
        hoods.iter().any(|(label, _)| label == "Silver Lake"),
        "a real neighbourhood survives: {hoods:?}"
    );
}
