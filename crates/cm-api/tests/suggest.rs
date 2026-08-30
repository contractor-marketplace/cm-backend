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
