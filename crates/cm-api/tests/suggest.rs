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

/// A place is one suggestion, whatever the postal service thinks.
///
/// Five ZCTAs are called Burbank. Offering five rows of numbers makes the
/// person pick one, and picking one searches a two-kilometre ZIP instead of
/// the city — while the four-per-kind cap silently drops the fifth, so the
/// city could not be searched whole even by choosing every row on offer.
///
/// So a word collapses to one row carrying its ZIPs, at the centre of them.
#[sqlx::test(migrations = "../../migrations")]
async fn a_place_is_one_suggestion_however_many_zips_it_has(pool: PgPool) {
    seed(&pool).await;
    let mut conn = pool.acquire().await.expect("connection");
    // Five Burbank ZCTAs in a rough line, so the average is the middle one.
    for (i, code) in ["91501", "91502", "91504", "91505", "91506"]
        .iter()
        .enumerate()
    {
        cm_db::repo::reference::upsert_zcta(
            &mut conn,
            code,
            "Burbank",
            34.16 + (i as f64) * 0.01,
            -118.32,
            None,
            "test",
        )
        .await
        .expect("zcta");
    }
    drop(conn);

    let mut client = Client::new(router(pool));
    let response = client.get("/v1/suggest?q=burbank").await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);

    let places: Vec<&serde_json::Value> = response.json["suggestions"]
        .as_array()
        .expect("array")
        .iter()
        .filter(|s| s["kind"] == "place")
        .collect();

    assert_eq!(
        places.len(),
        1,
        "one Burbank, not five: {:?}",
        response.json
    );
    assert_eq!(places[0]["label"], "Burbank");
    assert_eq!(places[0]["hint"], "5 ZIP codes");

    // The point is the middle of the five, so choosing "Burbank" searches from
    // the city rather than from whichever ZIP happened to sort first.
    let lat = places[0]["lat"].as_f64().expect("lat");
    let lon = places[0]["lon"].as_f64().expect("lon");
    assert!((lat - 34.18).abs() < 0.001, "lat was {lat}");
    assert!((lon + 118.32).abs() < 0.001, "lon was {lon}");
}

/// Typing digits is completing a code, so the codes are what come back.
///
/// The grouping above must not swallow this: somebody four characters into a
/// ZIP wants to see 91501 and 91502, not one row saying "Burbank".
#[sqlx::test(migrations = "../../migrations")]
async fn typing_a_number_offers_the_codes_themselves(pool: PgPool) {
    seed(&pool).await;
    let mut conn = pool.acquire().await.expect("connection");
    for code in ["91501", "91502"] {
        cm_db::repo::reference::upsert_zcta(
            &mut conn, code, "Burbank", 34.18, -118.32, None, "test",
        )
        .await
        .expect("zcta");
    }
    drop(conn);

    let mut client = Client::new(router(pool));
    let response = client.get("/v1/suggest?q=9150").await;
    let places: Vec<(&str, &str)> = response.json["suggestions"]
        .as_array()
        .expect("array")
        .iter()
        .filter(|s| s["kind"] == "place")
        .map(|s| {
            (
                s["label"].as_str().expect("label"),
                s["hint"].as_str().unwrap_or(""),
            )
        })
        .collect();

    assert_eq!(places.len(), 2, "both codes: {:?}", response.json);
    assert!(places.iter().any(|(l, h)| *l == "91501" && *h == "Burbank"));
    assert!(places.iter().any(|(l, h)| *l == "91502" && *h == "Burbank"));
}

/// Every place answers where it is, so choosing one needs no second lookup.
/// The client used to resolve a ZIP against all 1,763 regions it had loaded,
/// which cannot work for a city — "Burbank" has no code to look up.
#[sqlx::test(migrations = "../../migrations")]
async fn a_place_carries_its_own_point(pool: PgPool) {
    seed(&pool).await;
    let mut client = Client::new(router(pool));

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

/// A namesake ninety miles away is a different place, not an outlier.
///
/// Ten ZCTAs are called Glendale: nine around Los Angeles and 92105 in San
/// Diego. Averaging all ten put "Glendale" at 34.018, -118.139 — open ground
/// near Montebello, in neither city. A search from there answers for nowhere,
/// and nothing on the page would have said so.
#[sqlx::test(migrations = "../../migrations")]
async fn a_far_away_namesake_does_not_drag_a_place_off_itself(pool: PgPool) {
    seed(&pool).await;
    let mut conn = pool.acquire().await.expect("connection");
    for (code, lat, lon) in [
        ("91201", 34.1705, -118.2895),
        ("91202", 34.1684, -118.2678),
        ("91203", 34.1533, -118.2630),
        // San Diego, and also called Glendale.
        ("92105", 32.7378, -117.0927),
    ] {
        cm_db::repo::reference::upsert_zcta(&mut conn, code, "Glendale", lat, lon, None, "test")
            .await
            .expect("zcta");
    }
    drop(conn);

    let mut client = Client::new(router(pool));
    let response = client.get("/v1/suggest?q=glendale").await;
    let places: Vec<&serde_json::Value> = response.json["suggestions"]
        .as_array()
        .expect("array")
        .iter()
        .filter(|s| s["kind"] == "place")
        .collect();

    // Two places, largest first — not one average of both.
    assert_eq!(places.len(), 2, "two Glendales: {:?}", response.json);
    assert_eq!(places[0]["hint"], "3 ZIP codes");

    // The Los Angeles one sits on Los Angeles, not between the two.
    let lat = places[0]["lat"].as_f64().expect("lat");
    let lon = places[0]["lon"].as_f64().expect("lon");
    assert!((34.15..34.18).contains(&lat), "lat was {lat}");
    assert!((-118.30..-118.25).contains(&lon), "lon was {lon}");

    // And the San Diego one is still reachable, on itself.
    assert_eq!(places[1]["hint"], "ZIP 92105");
    assert!((places[1]["lat"].as_f64().expect("lat") - 32.7378).abs() < 0.001);
}
