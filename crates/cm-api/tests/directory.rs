//! The public contractor directory: search, map and detail.

mod common;

use common::{contractor_id, force_claim, router, seed_directory, user_id, Client};
use http::StatusCode;
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn the_directory_lists_contractors_without_a_session(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    let response = client.get("/v1/contractors").await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);

    let contractors = response.json["contractors"].as_array().expect("array");
    assert_eq!(contractors.len(), 4);
    assert!(contractors[0]["display_name"].is_string());
    assert!(contractors[0]["lat"].is_number(), "seeded at ZIP precision");
    assert_eq!(contractors[0]["location_precision"], "zip_centroid");
}

/// No read path selects `precise_point`, only the published one.
///
/// Since 0019 the two usually hold the same coordinates, because the licence
/// address is a public record and the directory publishes it. This test writes
/// a precise point WITHOUT republishing, so the two deliberately disagree, and
/// then checks the published point is what comes back. That separation is what
/// a `protected` listing relies on, and it is why search and map can never
/// disagree about where somebody is.
#[sqlx::test(migrations = "../../migrations")]
async fn reads_return_the_published_point_and_never_the_precise_column(pool: PgPool) {
    seed_directory(&pool).await;

    // Give one contractor a precise point that differs from its published one,
    // by writing the column directly rather than going through `republish`.
    sqlx::query(
        "UPDATE contractors SET precise_point = ST_SetSRID(ST_MakePoint(-118.18751, 34.11801), 4326)::geography \
          WHERE postal_code = '90042'",
    )
    .execute(&pool)
    .await
    .expect("set a precise point");

    let mut client = Client::new(router(pool));

    for path in [
        "/v1/contractors",
        "/v1/contractors/map?bbox=-119,33,-117,35",
        "/v1/contractors?q=ibarra",
        "/v1/contractors?lat=34.1156&lon=-118.1926&radius_m=50000",
    ] {
        let response = client.get(path).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path}: {:?}",
            response.json
        );

        let rendered = response.json.to_string();
        for leaked in ["34.11801", "-118.18751", "precise"] {
            assert!(
                !rendered.contains(leaked),
                "{path} leaked {leaked}: {rendered}"
            );
        }
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn text_search_matches_names_and_tolerates_partial_words(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    for (query, expected) in [
        ("ibarra", "Ibarra & Daughters Construction"),
        ("meridian", "Meridian Electric Co"),
        ("Stillwater", "Stillwater Plumbing"),
    ] {
        let response = client.get(&format!("/v1/contractors?q={query}")).await;
        let names: Vec<&str> = response.json["contractors"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|c| c["display_name"].as_str())
            .collect();

        assert!(names.contains(&expected), "{query} returned {names:?}");
    }

    let nothing = client.get("/v1/contractors?q=zzzzznotarealbusiness").await;
    assert_eq!(
        nothing.json["contractors"].as_array().expect("array").len(),
        0
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn filters_narrow_the_directory(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool.clone()));

    // By trade.
    let electricians = client.get("/v1/contractors?trade=electrician").await;
    let names: Vec<&str> = electricians.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert_eq!(names, vec!["Meridian Electric Co"]);

    // By ZIP.
    let by_zip = client.get("/v1/contractors?zip=90042").await;
    assert_eq!(
        by_zip.json["contractors"].as_array().expect("array").len(),
        1
    );

    // By distance: a tight ring on Santa Monica finds only what is there.
    let near = client
        .get("/v1/contractors?lat=34.0195&lon=-118.4912&radius_m=2000&sort=distance")
        .await;
    let names: Vec<&str> = near.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert_eq!(names, vec!["Stillwater Plumbing"]);
    assert!(near.json["contractors"][0]["distance_m"].is_number());

    // Verified-only is empty until something is both claimed and licensed.
    let verified = client.get("/v1/contractors?verified=true").await;
    assert_eq!(
        verified.json["contractors"]
            .as_array()
            .expect("array")
            .len(),
        0
    );

    let owner = cm_core::new_id();
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::insert(
        &mut conn,
        owner,
        Some("owner@example.test"),
        "Owner",
        cm_db::repo::users::AccountType::Contractor,
    )
    .await
    .expect("user");
    drop(conn);
    force_claim(&pool, contractor_id(&pool, "1047382").await, owner).await;

    let verified = client.get("/v1/contractors?verified=true").await;
    let names: Vec<&str> = verified.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert_eq!(names, vec!["Ibarra & Daughters Construction"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn paging_walks_the_whole_directory_exactly_once(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;

    for _ in 0..10 {
        let path = match &cursor {
            Some(c) => format!("/v1/contractors?limit=2&cursor={c}"),
            None => "/v1/contractors?limit=2".to_owned(),
        };
        let response = client.get(&path).await;
        assert_eq!(response.status, StatusCode::OK);

        for contractor in response.json["contractors"].as_array().expect("array") {
            seen.push(contractor["id"].as_str().expect("id").to_owned());
        }

        match response.json["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }

    assert_eq!(seen.len(), 4, "every contractor appears");
    let unique: std::collections::HashSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), 4, "and none appears twice");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_junk_filter_is_dropped_and_reported_rather_than_failing_the_page(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    let response = client.get("/v1/contractors?zip=banana&bbox=nonsense").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.json["contractors"]
            .as_array()
            .expect("array")
            .len(),
        4
    );

    let ignored: Vec<&str> = response.json["ignored_filters"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(ignored, vec!["zip", "bbox"]);

    // A structural parameter is different: getting the wrong page silently
    // would look like data loss.
    assert_eq!(
        client.get("/v1/contractors?limit=banana").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client.get("/v1/contractors?cursor=!!!").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client.get("/v1/contractors?sort=distance").await.status,
        StatusCode::BAD_REQUEST,
        "distance from nowhere is meaningless"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_map_shares_the_directory_predicate_and_caps_honestly(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    let response = client
        .get("/v1/contractors/map?bbox=-119,33,-117,35&trade=electrician")
        .await;
    assert_eq!(response.status, StatusCode::OK);

    let points = response.json["points"].as_array().expect("array");
    assert_eq!(points.len(), 1, "the map filters exactly as the list does");
    assert_eq!(points[0]["display_name"], "Meridian Electric Co");
    assert_eq!(response.json["truncated"], false);
    assert!(response.json["limit"].is_number());

    // A viewport over the ocean is empty rather than an error.
    let empty = client.get("/v1/contractors/map?bbox=-160,10,-150,20").await;
    assert_eq!(empty.json["points"].as_array().expect("array").len(), 0);
}

/// A pin's hover card says what the row says, or says nothing.
///
/// The rating rides along on the map payload so a pin can show it without the
/// client paging to the row — around 1% of listings carry one. The absence has
/// to stay an absence: sent as `null` it reads as a number to anything doing a
/// truthiness check, and "no reviews collected" and "rated zero" are different
/// claims that this product exists to keep apart.
#[sqlx::test(migrations = "../../migrations")]
async fn a_map_point_carries_a_rating_only_where_there_is_one(pool: PgPool) {
    seed_directory(&pool).await;

    sqlx::query(
        "UPDATE contractors SET google_rating = 4.9, google_review_count = 508 \
          WHERE display_name = 'Meridian Electric Co'",
    )
    .execute(&pool)
    .await
    .expect("set a rating");

    let mut client = Client::new(router(pool));
    let response = client.get("/v1/contractors/map?bbox=-119,33,-117,35").await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);

    let points = response.json["points"].as_array().expect("array");
    let rated = points
        .iter()
        .find(|p| p["display_name"] == "Meridian Electric Co")
        .expect("the rated contractor is on the map");
    assert_eq!(rated["google_rating"], 4.9);
    assert_eq!(rated["google_review_count"], 508);

    for point in points {
        if point["display_name"] == "Meridian Electric Co" {
            continue;
        }
        assert!(
            point.get("google_rating").is_none(),
            "an unrated listing sends no rating at all: {point}"
        );
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_detail_page_resolves_by_id_or_slug_and_shows_its_evidence(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let mut client = Client::new(router(pool));

    let by_id = client.get(&format!("/v1/contractors/{id}")).await;
    assert_eq!(by_id.status, StatusCode::OK, "{:?}", by_id.json);
    assert_eq!(by_id.json["license_no"], "1047382");
    assert_eq!(by_id.json["license_status"], "active");
    assert_eq!(by_id.json["verified"], false);

    // The evidence behind the badge is visible, so "why is this not verified"
    // is answerable from the page itself.
    assert!(!by_id.json["verification"]
        .as_array()
        .expect("array")
        .is_empty());

    let slug = by_id.json["slug"].as_str().expect("slug").to_owned();
    let by_slug = client.get(&format!("/v1/contractors/{slug}")).await;
    assert_eq!(by_slug.json["id"], by_id.json["id"]);

    assert_eq!(
        client.get("/v1/contractors/does-not-exist").await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn only_the_claimant_may_edit_a_listing_and_never_its_badge(pool: PgPool) {
    seed_directory(&pool).await;
    let id = contractor_id(&pool, "1047382").await;
    let router = router(pool.clone());

    let mut owner = Client::new(router.clone());
    owner.register_contractor("owner@example.test").await;
    let mut stranger = Client::new(router.clone());
    stranger.register("stranger@example.test").await;

    // Before claiming, even the eventual owner cannot edit.
    let refused = owner
        .send(
            http::Method::PATCH,
            &format!("/v1/contractors/{id}"),
            Some(serde_json::json!({ "bio": "mine now" })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    force_claim(&pool, id, user_id(&pool, "owner@example.test").await).await;

    let edited = owner
        .send(
            http::Method::PATCH,
            &format!("/v1/contractors/{id}"),
            Some(serde_json::json!({ "bio": "Second-generation GC", "accepts_dm": true })),
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.json);
    assert_eq!(edited.json["bio"], "Second-generation GC");
    assert_eq!(edited.json["accepts_dm"], true);

    // A stranger still cannot.
    let refused = stranger
        .send(
            http::Method::PATCH,
            &format!("/v1/contractors/{id}"),
            Some(serde_json::json!({ "bio": "not mine" })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // And nobody may set the badge — refused outright, not ignored.
    let rejected = owner
        .send(
            http::Method::PATCH,
            &format!("/v1/contractors/{id}"),
            Some(serde_json::json!({ "verified": true })),
        )
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    assert!(rejected.json["error"]["message"]
        .as_str()
        .expect("message")
        .contains("computed"));
}

/// A trade filter nobody offers must say so, because the alternative is worse
/// than an error: an unresolved slug leaves an empty id set, which every layer
/// below reads as "no trade filter", so `?trade=banana` used to return every
/// contractor in the county while reporting nothing. Of all the optional
/// filters it was the only one that could not explain itself.
#[sqlx::test(migrations = "../../migrations")]
async fn an_unknown_trade_reports_itself_instead_of_widening_the_search(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    let response = client.get("/v1/contractors?trade=banana").await;
    assert_eq!(response.status, StatusCode::OK);

    let ignored: Vec<&str> = response.json["ignored_filters"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        ignored.contains(&"trade"),
        "an unresolved trade must be reported: {:?}",
        response.json
    );

    // A filter that partly resolves is still doing its job, and saying "trade"
    // there would make the client clear a control that is working.
    let mixed = client.get("/v1/contractors?trade=plumber,banana").await;
    assert_eq!(mixed.status, StatusCode::OK);
    let names: Vec<&str> = mixed.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert_eq!(names, vec!["Stillwater Plumbing"], "{names:?}");
    assert!(
        !mixed.json["ignored_filters"]
            .as_array()
            .map(|a| a.iter().any(|v| v.as_str() == Some("trade")))
            .unwrap_or(false),
        "a partly resolved trade filter is not an ignored one: {:?}",
        mixed.json
    );
}

/// A licence in a class the picker does not offer still carries that trade.
/// The importer reads every trade; only the directory's filter list is
/// shortened. Getting this backwards would mean a C-20 licence imports with no
/// trade purely because "Heating & Air Conditioning" was not in a dropdown.
#[sqlx::test(migrations = "../../migrations")]
async fn an_unfeatured_classification_is_still_imported_as_a_trade(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::reference::seed_trades(&mut conn)
        .await
        .expect("trades");

    let offered = cm_db::repo::reference::all_trades(&mut conn)
        .await
        .expect("offered");
    let importable = cm_db::repo::reference::all_trades_for_import(&mut conn)
        .await
        .expect("importable");

    assert!(
        importable.len() > offered.len(),
        "the import set must be the wider one: {} vs {}",
        importable.len(),
        offered.len()
    );
    assert!(
        importable
            .iter()
            .any(|trade| trade.cslb_classification.as_deref() == Some("C-11")),
        "an unfeatured classification is missing from the import set"
    );
    assert!(
        !offered
            .iter()
            .any(|trade| trade.cslb_classification.as_deref() == Some("C-11")),
        "an unfeatured classification must not reach the picker"
    );
}

/// Every sort has to paginate, and the reason this is asserted for all of them
/// at once is that it used not to be true for any but one.
///
/// The cursor carried `(display_name, id)` whatever the ORDER BY led with, so
/// under `sort=distance` page two filtered on a column it was not ordered by
/// and dropped rows on the floor. The front end worked around it by refusing to
/// paginate those sorts, capping them at fifty results with no way past.
///
/// Walking a page at a time and demanding every contractor exactly once is the
/// cheapest statement of "the cursor and the ordering agree", and it fails on
/// both halves of the old bug: a mismatched key loses rows, and a mismatched
/// direction repeats them.
#[sqlx::test(migrations = "../../migrations")]
async fn every_sort_pages_through_the_whole_directory_exactly_once(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    let expected = 4;
    for sort in [
        "",
        "?sort=best",
        "?sort=rating",
        "?sort=name",
        "?sort=relevance",
        "?q=co&sort=best",
        "?lat=34.05&lon=-118.30&radius_m=200000&sort=distance",
    ] {
        let base = if sort.is_empty() {
            "/v1/contractors?limit=1".to_owned()
        } else {
            format!("/v1/contractors{sort}&limit=1")
        };

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..12 {
            let path = match &cursor {
                Some(value) => format!("{base}&cursor={value}"),
                None => base.clone(),
            };
            let response = client.get(&path).await;
            assert_eq!(
                response.status,
                StatusCode::OK,
                "{path}: {:?}",
                response.json
            );

            for contractor in response.json["contractors"].as_array().expect("array") {
                seen.push(contractor["id"].as_str().expect("id").to_owned());
            }
            match response.json["next_cursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }

        let unique: std::collections::HashSet<&String> = seen.iter().collect();
        assert_eq!(
            seen.len(),
            unique.len(),
            "{sort:?} returned a contractor twice: {seen:?}"
        );

        // `?q=co` is a filter, so it is allowed to match fewer than everything;
        // what it may not do is lose or repeat one while paging.
        if !sort.contains("q=") {
            assert_eq!(
                seen.len(),
                expected,
                "{sort:?} paged through {} of {expected} contractors",
                seen.len()
            );
        }
    }
}

/// A cursor issued before the sort key joined it is refused rather than
/// half-understood. Resuming a scored ordering from a name is the defect the
/// key was added to fix, so accepting the old shape would reintroduce it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_cursor_from_the_previous_shape_is_refused(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    use base64::Engine;
    let old_shape = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("{}\u{0}Ibarra & Daughters", uuid::Uuid::now_v7()));

    let response = client
        .get(&format!("/v1/contractors?cursor={old_shape}"))
        .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        response.json
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn reference_data_is_public(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool));

    let trades = client.get("/v1/trades").await;
    assert_eq!(trades.status, StatusCode::OK);

    // The picker offers the featured trades, not the whole classification set:
    // a homeowner choosing between 75 entries that open with "Air and Water
    // Balancing" is reading a haystack. Everything unfeatured is still matched
    // on import — see `all_trades_for_import`.
    let offered = trades.json.as_array().expect("array");
    let featured = cm_db::repo::reference::CANONICAL_TRADES
        .iter()
        .filter(|trade| trade.featured)
        .count();
    assert_eq!(offered.len(), featured);

    let slugs: Vec<&str> = offered
        .iter()
        .filter_map(|trade| trade["slug"].as_str())
        .collect();
    assert_eq!(
        slugs.first(),
        Some(&"general-contractor"),
        "the picker leads with what is searched most: {slugs:?}"
    );
    for expected in ["plumber", "electrician", "hvac", "roofer"] {
        assert!(slugs.contains(&expected), "{expected} is not offered");
    }
    assert!(
        !slugs.contains(&"wood-tanks"),
        "an unfeatured trade must not reach the picker: {slugs:?}"
    );

    let regions = client.get("/v1/regions").await;
    assert_eq!(regions.status, StatusCode::OK);
    assert_eq!(regions.json.as_array().expect("array").len(), 4);
}

/// The licence address is published, which is the point of the directory.
///
/// It comes from `license_records` rather than from anything a contractor
/// typed, so it is the address the CSLB register already publishes. Asserted on
/// every read surface, because a field that appears in the list and not on the
/// detail page is the sort of gap nobody notices until somebody asks why.
#[sqlx::test(migrations = "../../migrations")]
async fn the_directory_publishes_the_address_on_the_licence(pool: PgPool) {
    seed_directory(&pool).await;
    let mut client = Client::new(router(pool.clone()));

    let listed = client.get("/v1/contractors").await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.json);

    let first = &listed.json["contractors"][0];
    let street = first["address_line1"]
        .as_str()
        .expect("the list carries the street line");
    assert!(!street.is_empty());
    assert!(first["address_city"].is_string(), "and the city");

    // The detail page agrees.
    let slug = first["slug"].as_str().expect("slug");
    let detail = client.get(&format!("/v1/contractors/{slug}")).await;
    assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.json);
    assert_eq!(detail.json["address_line1"], street);

    // And so does the map, which labels a single pin with it.
    let map = client.get("/v1/contractors/map?bbox=-119,33,-117,35").await;
    assert_eq!(map.status, StatusCode::OK, "{:?}", map.json);
    assert!(
        map.json["points"]
            .as_array()
            .expect("array")
            .iter()
            .any(|p| p["address_line1"].is_string()),
        "a map pin should carry the street line: {:?}",
        map.json
    );
}

/// The point of the whole feature: a contractor is findable where they work,
/// not only where their licence says they are.
///
/// The directory has always matched a search against the address on a licence,
/// which is a poor proxy — a business that covers the west side is invisible to
/// somebody searching the next ZIP over, and a sole trader whose licence
/// carries their home address is placed at their house rather than their patch.
#[sqlx::test(migrations = "../../migrations")]
async fn a_contractor_is_found_in_an_area_they_serve_but_are_not_in(pool: PgPool) {
    seed_directory(&pool).await;

    // Give the ZIPs an area-equivalent radius, which is what stands in for a
    // boundary until polygons are loaded.
    sqlx::query("UPDATE regions SET approx_radius_m = 3000 WHERE kind = 'zcta'")
        .execute(&pool)
        .await
        .expect("radii");

    // Stillwater Plumbing is in Santa Monica (90401).
    let stillwater = contractor_id(&pool, "771204").await;
    let mut client = Client::new(router(pool.clone()));
    client.register_contractor("stillwater@example.com").await;
    let user = user_id(&pool, "stillwater@example.com").await;
    force_claim(&pool, stillwater, user).await;

    // Well outside their own travel radius, so the only thing that can bring
    // them back is declaring the area. Santa Monica to Silver Lake is about
    // 22km, which 0030's 25-mile default already covers — the point of this
    // test is the named region, so the distance has to be past that.
    sqlx::query(
        "UPDATE contractors \
            SET public_point = ST_SetSRID(ST_MakePoint(-119.4, 34.0781), 4326)::geography \
          WHERE id = $1",
    )
    .bind(stillwater)
    .execute(&pool)
    .await
    .expect("reposition");

    let silver_lake = (34.0781, -118.2606);
    let nearby = |lat: f64, lon: f64| format!("/v1/contractors?lat={lat}&lon={lon}");

    let before = client.get(&nearby(silver_lake.0, silver_lake.1)).await;
    let names: Vec<&str> = before.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Stillwater Plumbing"),
        "not there yet: {names:?}"
    );

    // They say they cover it.
    let saved = client
        .send(
            http::Method::PUT,
            &format!("/v1/contractors/{stillwater}/service-areas"),
            Some(serde_json::json!({ "areas": [{ "kind": "region", "code": "90026" }] })),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.json);

    let after = client.get(&nearby(silver_lake.0, silver_lake.1)).await;
    let names: Vec<&str> = after.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert!(
        names.contains(&"Stillwater Plumbing"),
        "declaring the area must make them findable in it: {names:?}"
    );

    // And the map agrees, because it runs the same predicate.
    let map = client
        .get("/v1/contractors/map?lat=34.0781&lon=-118.2606")
        .await;
    let mapped: Vec<&str> = map.json["points"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|p| p["display_name"].as_str())
        .collect();
    assert!(mapped.contains(&"Stillwater Plumbing"), "{mapped:?}");
}

/// Widening the travel radius past the default reaches further than the default
/// does.
///
/// The search carries no radius of its own, which is now the ordinary case: a
/// location alone asks who covers it. An explicit `radius_m` would narrow this
/// result rather than widen it — see
/// `a_search_radius_narrows_coverage_rather_than_defining_it`.
#[sqlx::test(migrations = "../../migrations")]
async fn a_travel_radius_widens_the_area_a_contractor_matches(pool: PgPool) {
    seed_directory(&pool).await;

    let stillwater = contractor_id(&pool, "771204").await;

    // Past the 25-mile default, so only an explicit widening can reach back.
    sqlx::query(
        "UPDATE contractors \
            SET public_point = ST_SetSRID(ST_MakePoint(-119.4, 34.0781), 4326)::geography \
          WHERE id = $1",
    )
    .bind(stillwater)
    .execute(&pool)
    .await
    .expect("reposition");

    let mut client = Client::new(router(pool.clone()));
    client.register_contractor("radius@example.com").await;
    let user = user_id(&pool, "radius@example.com").await;
    force_claim(&pool, stillwater, user).await;

    let missing = client
        .get("/v1/contractors?lat=34.0781&lon=-118.2606")
        .await;
    let before: Vec<&str> = missing.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert!(
        !before.contains(&"Stillwater Plumbing"),
        "the default radius must not already reach: {before:?}"
    );

    let saved = client
        .send(
            http::Method::PUT,
            &format!("/v1/contractors/{stillwater}/service-areas"),
            Some(serde_json::json!({ "areas": [{ "kind": "radius", "radius_m": 150000 }] })),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.json);

    let found = client
        .get("/v1/contractors?lat=34.0781&lon=-118.2606")
        .await;
    let names: Vec<&str> = found.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert!(names.contains(&"Stillwater Plumbing"), "{names:?}");
}

/// Only the claimant sets where their listing works, and the input is bounded.
#[sqlx::test(migrations = "../../migrations")]
async fn service_areas_belong_to_the_claimant_and_are_bounded(pool: PgPool) {
    seed_directory(&pool).await;
    let stillwater = contractor_id(&pool, "771204").await;
    let path = format!("/v1/contractors/{stillwater}/service-areas");

    // A stranger, signed in but owning nothing.
    let mut stranger = Client::new(router(pool.clone()));
    stranger.register("stranger@example.com").await;
    assert_eq!(stranger.get(&path).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        stranger
            .send(
                http::Method::PUT,
                &path,
                Some(serde_json::json!({ "areas": [] })),
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // No session at all.
    let mut anonymous = Client::new(router(pool.clone()));
    anonymous.clear_jar();
    assert_eq!(anonymous.get(&path).await.status, StatusCode::UNAUTHORIZED);

    let mut owner = Client::new(router(pool.clone()));
    owner.register_contractor("owner@example.com").await;
    let user = user_id(&pool, "owner@example.com").await;
    force_claim(&pool, stillwater, user).await;

    // Two radii is a contradiction, and the larger would win silently.
    let two = owner
        .send(
            http::Method::PUT,
            &path,
            Some(serde_json::json!({ "areas": [
                { "kind": "radius", "radius_m": 10000 },
                { "kind": "radius", "radius_m": 90000 }
            ]})),
        )
        .await;
    assert_eq!(two.status, StatusCode::BAD_REQUEST, "{:?}", two.json);

    // A ZIP the Census draws no area around is reported, not refused: the
    // gazetteer covers populated places, and a PO-box code a contractor really
    // uses has no centroid to match against.
    let mixed = owner
        .send(
            http::Method::PUT,
            &path,
            Some(serde_json::json!({ "areas": [
                { "kind": "region", "code": "90026" },
                { "kind": "region", "code": "90209" }
            ]})),
        )
        .await;
    assert_eq!(mixed.status, StatusCode::OK, "{:?}", mixed.json);
    let unknown: Vec<&str> = mixed.json["unknown"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(unknown, vec!["90209"]);

    // Two, not one: the region that resolved, plus the travel radius, which
    // every contractor has whether or not this request mentioned one. Sending
    // no radius means "leave it at the default", never "I travel nowhere".
    let areas = mixed.json["areas"].as_array().expect("array");
    assert_eq!(areas.len(), 2, "{areas:?}");
    assert!(
        areas.iter().any(|a| a["kind"] == "radius"),
        "a saved listing always reports the radius in force: {areas:?}"
    );
}

/// Coverage, not proximity.
///
/// The directory used to answer "which contractors are near this address",
/// which is a different question from the one a homeowner is asking. These
/// three cases are where the two answers diverge, and all three have to hold
/// together — each one alone is satisfiable by a predicate that gets the others
/// wrong.
#[sqlx::test(migrations = "../../migrations")]
async fn a_search_finds_who_covers_the_address_not_who_is_near_it(pool: PgPool) {
    seed_directory(&pool).await;

    // Santa Monica. Everything below is positioned relative to this.
    const LAT: f64 = 34.0195;
    const LON: f64 = -118.4912;

    // Roughly 60km east — outside anybody's default 25 miles (40.2km).
    let far = |conn: &PgPool, name: &str, radius_m: i32| {
        let name = name.to_owned();
        let conn = conn.clone();
        async move {
            sqlx::query(
                "UPDATE contractors \
                    SET public_point = ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography, \
                        service_radius_m = $4 \
                  WHERE display_name = $1",
            )
            .bind(&name)
            .bind(LON + 0.65)
            .bind(LAT)
            .bind(radius_m)
            .execute(&conn)
            .await
            .expect("reposition");
        }
    };

    // Far away, travels far: covers Santa Monica.
    far(&pool, "Ibarra & Daughters Construction", 100_000).await;
    // Far away, ordinary radius: does not.
    far(&pool, "Meridian Electric Co", 40_234).await;

    // Close by, but only works its own street: must not be dragged in by the
    // constant-radius branch. This is the case the equality guard exists for —
    // without it a narrowed radius is silently ignored.
    sqlx::query(
        "UPDATE contractors \
            SET public_point = ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography, \
                service_radius_m = 2000 \
          WHERE display_name = $1",
    )
    .bind("Stillwater Plumbing")
    .bind(LON + 0.05)
    .bind(LAT)
    .execute(&pool)
    .await
    .expect("reposition");

    let mut client = Client::new(router(pool));
    let response = client
        .get(&format!("/v1/contractors?lat={LAT}&lon={LON}"))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);

    let names: Vec<&str> = response.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();

    assert!(
        names.contains(&"Ibarra & Daughters Construction"),
        "a contractor 60km away who travels 100km covers this address: {names:?}"
    );
    assert!(
        !names.contains(&"Meridian Electric Co"),
        "a contractor 60km away on the default radius does not: {names:?}"
    );
    assert!(
        !names.contains(&"Stillwater Plumbing"),
        "a contractor who narrowed their radius to 2km must not be matched by \
         the default-radius branch just for being close: {names:?}"
    );
    assert!(
        names.contains(&"Reinholt Roofing"),
        "a listing nobody has claimed still covers its own neighbourhood: {names:?}"
    );
}

/// A radius on the query narrows what coverage returned; it does not replace it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_search_radius_narrows_coverage_rather_than_defining_it(pool: PgPool) {
    seed_directory(&pool).await;

    const LAT: f64 = 34.0195;
    const LON: f64 = -118.4912;

    sqlx::query(
        "UPDATE contractors \
            SET public_point = ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, \
                service_radius_m = 100000 \
          WHERE display_name = 'Ibarra & Daughters Construction'",
    )
    .bind(LON + 0.65)
    .bind(LAT)
    .execute(&pool)
    .await
    .expect("reposition");

    let mut client = Client::new(router(pool));

    let covered = client
        .get(&format!("/v1/contractors?lat={LAT}&lon={LON}"))
        .await;
    let names: Vec<&str> = covered.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert!(
        names.contains(&"Ibarra & Daughters Construction"),
        "{names:?}"
    );

    // The same search, restricted to 10km. The far contractor still covers the
    // address, but the searcher has asked for the close ones only.
    let narrowed = client
        .get(&format!(
            "/v1/contractors?lat={LAT}&lon={LON}&radius_m=10000"
        ))
        .await;
    let narrowed_names: Vec<&str> = narrowed.json["contractors"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["display_name"].as_str())
        .collect();
    assert!(
        !narrowed_names.contains(&"Ibarra & Daughters Construction"),
        "an explicit radius must exclude the far coverage: {narrowed_names:?}"
    );
}
