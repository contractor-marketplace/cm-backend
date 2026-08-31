//! Search relevance, measured rather than argued about.
//!
//! Every other test in this suite asks whether search returns the *right set*.
//! This one asks whether it returns them in the *right order*, which is the
//! question a ranking change is actually about and the one a pass/fail
//! assertion cannot answer on its own.
//!
//! The shape is a graded golden set — `tests/fixtures/search_golden.jsonl`,
//! queries with hand-labelled relevance judgements — scored with NDCG@10 and
//! Recall@20 against the fixed corpus below. Relevance cannot be measured
//! without knowing the right answer in advance, and only a person can say what
//! the right answer is; that is what the judgements are.
//!
//! The floors are **measurements, not aspirations**: each was read off a run and
//! pinned so it can only go up. A ranking change that improves the first page
//! raises them; one that quietly trades recall for precision fails here instead
//! of shipping. Run with `--nocapture` for the per-query table — the mean moving
//! is the alarm, the table is the diagnosis — and raise the floors in the same
//! commit as the change that earned them.

use cm_db::repo::search::{self, Filters, Sort};
use cm_db::PgPool;
use std::collections::HashMap;
use std::path::Path;

/* ── The floors ────────────────────────────────────────────────────────────
 * Measured on this corpus, 2026-08-29, against search as it stands: mean
 * NDCG@10 0.468, mean Recall@20 0.471, name-query NDCG@10 0.833. The constants
 * sit just under those, so ordinary noise does not fail the build but a real
 * regression does.
 *
 * Three things the first run found. The first is fixed; the other two are what
 * the later phases exist to close. They are recorded here rather than in a
 * ticket because this is the file that shows them being fixed:
 *
 *   1. FIXED, in two steps. `ibara` — one letter dropped from "Ibarra" —
 *      returned NOTHING. The fuzzy clause was `c.display_name % $4`, and `%`
 *      compares the query against the WHOLE column: 0.161 against a 0.3
 *      threshold. The fuzzy matching the architecture doc advertises only
 *      worked when the query approximated the entire business name, which is
 *      not how anyone types.
 *
 *      Switching to `<%` (word similarity) took the mean to 0.607/0.623, and
 *      lowering `word_similarity_threshold` from pg_trgm's default 0.6 to 0.5
 *      took it to 0.644/0.667. The threshold was chosen by measurement, not
 *      taste: against 40 real CSLB business names each missing one character,
 *      0.6 found 7 and 0.5 found all 40.
 *
 *      What 0.5 costs is visible in the table: `solar` returns one row that is
 *      not Helios Power Systems. It is "Polar Air Heating & Cooling", scoring
 *      exactly 0.500 — "polar" and "solar" differ by a letter, and no
 *      threshold that catches a one-character typo can also reject one. It
 *      ranks last and will fall below the real match once `solar` routes to
 *      C-46 through the taxonomy, which is the actual fix for that row.
 *
 *   2. FIXED. `ibarra` and `meridian electric` scored 0.834 rather than 1.0.
 *      Both queries match two businesses, `ts_rank` tied them, and the tiebreak
 *      was alphabetical — which under the database collation ignores
 *      punctuation and case, putting "Ibarra Brothers" (3.8 stars, 20 reviews,
 *      unclaimed) above "Ibarra & Daughters" (4.5 stars, 95 reviews, verified).
 *
 *      Ranking now blends text relevance with a standing quality score, and the
 *      order of the terms is the whole design: naming a business beats being
 *      that kind of contractor, which beats having a name one letter away from
 *      the word typed, and quality orders whatever is left level. That last
 *      ordering was itself measured — "solar" returned "Polar Air Heating"
 *      above an actual solar contractor until a trade match was scored above a
 *      fuzzy name match.
 *
 *   3. FIXED. Every trade word and every natural-language query scored zero:
 *      water heater, rewire, hvac, adu, leaking pipe, solar. Nine of the
 *      twenty-three golden queries returned no rows at all, which was by some
 *      distance the largest number on the table.
 *
 *      Two causes, and only the second was the obvious one. The taxonomy held
 *      six of ~80 CSLB classifications, so a C-20 licence carried no trade to
 *      match; that is now 75, covering 98.9% of the register. But expanding it
 *      fixed the `?trade=` filter and changed the search box not at all,
 *      because free text is compared against a business name and a bio and no
 *      business is called "hvac". The gap was vocabulary, not retrieval:
 *      `trade_aliases` maps how a person describes a problem to how a licence
 *      is classified, and the query resolves through it before the search runs.
 *      Together: 0.644/0.667 -> 0.971/1.000.
 */

/// Mean NDCG@10 across the golden set. Measured: 1.000 over thirty-three
/// queries, ten of which are materially harder than the original set.
///
/// The road: 0.468 at the start, 0.607 with word similarity, 0.644 at the
/// measured threshold, 0.971 once queries routed through the trade vocabulary,
/// 1.000 once ranking blended standing quality with text relevance — then 0.836
/// when the hard queries were added, and 1.000 again once routing learned to
/// find an alias inside a sentence.
///
/// **What this says about semantic search.** The plan holds embeddings behind a
/// gate: ship only on a measured gain over what is already here. The hard
/// queries were added precisely to find that gap, and what they exposed was a
/// missing string comparison, not a missing model. Everything they asked for is
/// answered by a table of words and two similarity directions. There is no
/// gap for a vector index to close, so there is nothing to justify one — which
/// the plan names as a legitimate outcome rather than a deferral.
///
/// The set was exhausted once at twenty-three queries and was grown to
/// thirty-three: whole sentences ("someone to fix my leaking shower"), symptoms
/// rather than services ("ac not cooling"), regional slang ("granny flat"),
/// qualifiers the directory cannot honour ("cheapest roofer") and a misspelled
/// trade word rather than a misspelled business name ("plummer").
///
/// Those ten dropped the mean to 0.836, which is what a useful set of
/// judgements is for: they found that routing compared the *whole* query
/// against an alias, so a sentence never matched the short phrase inside it.
/// Adding the containment direction took it to 0.939, and two fixture
/// corrections — a finish-carpentry business the corpus was missing, and the
/// words a person uses for a shower — took it to 1.000.
///
/// **It is saturated again, and will need growing again.** At 1.000 it can only
/// detect regression, and "no worse" is not "no better".
const NDCG_FLOOR: f64 = 0.99;
/// Mean Recall@20. Measured: 1.000 — every golden query finds everything it
/// should. Pinned just under, so a single lost result fails the build.
const RECALL_FLOOR: f64 = 0.99;
/// Looking a business up by name is what search already did well, so it is
/// pinned separately and tightly — a change that chases recall and quietly
/// costs plain lookup fails here while the mean still looks healthy.
/// Measured: 1.000 (0.833 before ranking resolved the ties).
const NAME_QUERY_NDCG_FLOOR: f64 = 0.99;

/// Golden-set entries reachable from the business name alone. Everything else
/// needs the taxonomy, a synonym, or a field not yet in the search document.
const NAME_QUERIES: &[&str] = &[
    "ibarra",
    "ibara",
    "stillwater plumbing",
    "meridian electric",
    "reinholt",
    "summit roofing",
    "keystone",
    "verdant",
];

/* ── The corpus ────────────────────────────────────────────────────────────
 * The four businesses `cm-api/tests/common/mod.rs` already seeds, with their
 * licence numbers and ZIPs unchanged, extended to sixteen. Four rows cannot
 * distinguish good ranking from bad: with one plumber in the database every
 * plumbing query scores perfectly however the ordering works.
 *
 * The additions are chosen, not padding:
 *
 *   - Two pairs share a name ("Ibarra", "Meridian"), so a query matching both
 *     has a defensible order rather than a tie.
 *   - Ratings span 3.5 to 4.9 and review counts two orders of magnitude, so a
 *     blended ranking has something to blend and a lexical-only one visibly
 *     cannot.
 *   - One licence is expired, and trust signals vary, so `verified` means
 *     something across the set.
 *   - Two carry CSLB classes that are not seeded trades (C-20 HVAC, C-46
 *     solar) and so are unreachable by any trade filter. That is not a flaw in
 *     the fixture; it is the defect the taxonomy work fixes, and these two rows
 *     are how the fix gets measured.
 *
 * All of it lives in the throwaway database `#[sqlx::test]` builds per test.
 */
struct Seed {
    license_no: &'static str,
    name: &'static str,
    status: &'static str,
    postal_code: &'static str,
    classification: &'static str,
    bio: Option<&'static str>,
    rating: Option<f64>,
    reviews: Option<i32>,
    verified: bool,
}

#[rustfmt::skip]
const CORPUS: &[Seed] = &[
    Seed { license_no: "1047382", name: "Ibarra & Daughters Construction", status: "active",  postal_code: "90042", classification: "B",    bio: Some("Whole-home remodels and additions in northeast Los Angeles."), rating: Some(4.5), reviews: Some(95),  verified: true  },
    Seed { license_no: "1047383", name: "Ibarra Brothers Builders",        status: "active",  postal_code: "90026", classification: "B",    bio: None,                                                               rating: Some(3.8), reviews: Some(20),  verified: false },
    Seed { license_no: "1047384", name: "Keystone General Contracting",    status: "active",  postal_code: "90232", classification: "B",    bio: Some("Ground-up residential builds."),                               rating: Some(4.3), reviews: Some(120), verified: true  },
    Seed { license_no: "983311",  name: "Meridian Electric Co",            status: "active",  postal_code: "90232", classification: "C-10", bio: Some("Panel upgrades and service changes."),                         rating: Some(4.8), reviews: Some(210), verified: true  },
    Seed { license_no: "983312",  name: "Bright Spark Electric",           status: "active",  postal_code: "90026", classification: "C-10", bio: None,                                                               rating: Some(4.2), reviews: Some(35),  verified: false },
    Seed { license_no: "983313",  name: "Voltaire Electrical Services",    status: "active",  postal_code: "90042", classification: "C-10", bio: None,                                                               rating: Some(3.9), reviews: Some(12),  verified: false },
    Seed { license_no: "983314",  name: "Meridian Electrical Contractors", status: "active",  postal_code: "90401", classification: "C-10", bio: None,                                                               rating: Some(4.6), reviews: Some(88),  verified: false },
    Seed { license_no: "771204",  name: "Stillwater Plumbing",             status: "active",  postal_code: "90401", classification: "C-36", bio: Some("Repipes, drain clearing and fixture replacement."),            rating: Some(4.9), reviews: Some(320), verified: true  },
    Seed { license_no: "771205",  name: "Cascade Plumbing & Rooter",       status: "active",  postal_code: "90026", classification: "C-36", bio: None,                                                               rating: Some(4.1), reviews: Some(45),  verified: false },
    Seed { license_no: "771206",  name: "Delgado Plumbing Co",             status: "active",  postal_code: "90232", classification: "C-36", bio: None,                                                               rating: Some(4.4), reviews: Some(60),  verified: false },
    Seed { license_no: "445190",  name: "Reinholt Roofing",                status: "expired", postal_code: "90026", classification: "C-39", bio: None,                                                               rating: Some(3.5), reviews: Some(8),   verified: false },
    Seed { license_no: "445191",  name: "Summit Roofing Group",            status: "active",  postal_code: "90042", classification: "C-39", bio: Some("Tile, shingle and flat roofs."),                               rating: Some(4.7), reviews: Some(150), verified: true  },
    Seed { license_no: "553001",  name: "Coastline Painting",              status: "active",  postal_code: "90401", classification: "C-33", bio: None,                                                               rating: Some(4.0), reviews: Some(30),  verified: false },
    Seed { license_no: "662010",  name: "Verdant Landscape Design",        status: "active",  postal_code: "90232", classification: "C-27", bio: Some("Drought-tolerant gardens and irrigation."),                     rating: Some(4.6), reviews: Some(75),  verified: false },
    Seed { license_no: "880455",  name: "Polar Air Heating & Cooling",     status: "active",  postal_code: "90026", classification: "C-20", bio: None,                                                               rating: Some(4.7), reviews: Some(180), verified: false },
    Seed { license_no: "660412",  name: "Alder & Oak Cabinetmakers",       status: "active",  postal_code: "90232", classification: "C-6",  bio: Some("Kitchen cabinets, built-ins and trim."),                        rating: Some(4.6), reviews: Some(58),  verified: false },
    Seed { license_no: "991777",  name: "Helios Power Systems",            status: "active",  postal_code: "90401", classification: "C-46", bio: None,                                                               rating: Some(4.4), reviews: Some(64),  verified: false },
];

const ZIPS: &[(&str, &str, f64, f64)] = &[
    ("90026", "Silver Lake", 34.0781, -118.2606),
    ("90042", "Highland Park", 34.1156, -118.1926),
    ("90232", "Culver City", 34.0211, -118.3965),
    ("90401", "Santa Monica", 34.0195, -118.4912),
];

/* ── Metrics ───────────────────────────────────────────────────────────────*/

/// Discounted cumulative gain, with the usual exponential gain so that moving a
/// highly relevant result up counts for more than moving a marginal one up.
fn dcg(gains: &[f64]) -> f64 {
    gains
        .iter()
        .enumerate()
        .map(|(rank, gain)| (2f64.powf(*gain) - 1.0) / ((rank + 2) as f64).log2())
        .sum()
}

/// NDCG@k: the ranking that was returned, over the best ranking available.
/// 1.0 is the ideal order; 0.0 is nothing relevant in the top k.
///
/// A query with no relevant documents scores 1.0 when it correctly returns
/// nothing relevant — there is no better ordering than the empty one, and
/// scoring it 0 would punish the right answer.
fn ndcg_at(returned: &[String], judgements: &HashMap<String, f64>, k: usize) -> f64 {
    let gains: Vec<f64> = returned
        .iter()
        .take(k)
        .map(|key| judgements.get(key).copied().unwrap_or(0.0))
        .collect();

    let mut ideal: Vec<f64> = judgements.values().copied().collect();
    ideal.sort_by(|a, b| b.partial_cmp(a).expect("no NaN in judgements"));
    ideal.truncate(k);

    let best = dcg(&ideal);
    if best == 0.0 {
        // Nothing was relevant. Returning nothing relevant is correct.
        return if gains.iter().all(|gain| *gain == 0.0) {
            1.0
        } else {
            0.0
        };
    }

    // `+ 0.0` collapses negative zero, which IEEE division produces here and
    // which prints as "-0.000" in the report.
    dcg(&gains) / best + 0.0
}

/// Recall@k: how much of what should have been found was found at all. NDCG can
/// look healthy while a whole class of matches is missing, because it only ever
/// sees what was returned.
fn recall_at(returned: &[String], judgements: &HashMap<String, f64>, k: usize) -> f64 {
    let wanted: Vec<&String> = judgements
        .iter()
        .filter(|(_, gain)| **gain > 0.0)
        .map(|(key, _)| key)
        .collect();

    if wanted.is_empty() {
        return if returned.is_empty() { 1.0 } else { 0.0 };
    }

    let found = wanted
        .iter()
        .filter(|key| returned.iter().take(k).any(|got| got == **key))
        .count();

    found as f64 / wanted.len() as f64
}

/* ── The golden set ────────────────────────────────────────────────────────*/

struct GoldenQuery {
    query: String,
    intent: String,
    judgements: HashMap<String, f64>,
}

fn golden_set() -> Vec<GoldenQuery> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/search_golden.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));

    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("malformed golden line {line}: {error}"));

            let judgements = value["relevant"]
                .as_object()
                .expect("relevant is an object")
                .iter()
                .map(|(license, gain)| {
                    (
                        license.clone(),
                        gain.as_f64().expect("a relevance grade is a number"),
                    )
                })
                .collect();

            GoldenQuery {
                query: value["query"].as_str().expect("query").to_owned(),
                intent: value["intent"].as_str().expect("intent").to_owned(),
                judgements,
            }
        })
        .collect()
}

/* ── Seeding ───────────────────────────────────────────────────────────────*/

/// A connection carrying the session state `pool::connect` gives every
/// production connection.
///
/// `#[sqlx::test]` builds its own pool and never runs that hook, so a search
/// made without this measures a stricter fuzzy threshold than the one that
/// ships. The setting is per-session, so it has to be applied to the
/// connection the search actually runs on — setting it on the pool would land
/// on whichever connection happened to serve the `SET` and not necessarily the
/// one that serves the query.
async fn search_connection(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    let mut conn = pool.acquire().await.expect("connection");
    sqlx::query(&format!(
        "SET pg_trgm.word_similarity_threshold = {}",
        cm_db::repo::search::WORD_SIMILARITY_THRESHOLD
    ))
    .execute(&mut *conn)
    .await
    .expect("threshold");
    conn
}

async fn seed_corpus(pool: &PgPool) {
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::reference::seed_trades(&mut conn)
        .await
        .expect("trades");
    cm_db::repo::reference::seed_trade_aliases(&mut conn)
        .await
        .expect("aliases");

    for (code, name, lat, lon) in ZIPS {
        cm_db::repo::reference::upsert_zcta(&mut conn, code, name, *lat, *lon, None, "test")
            .await
            .expect("zcta");
    }

    let run_id = cm_db::repo::licenses::begin_run(
        &mut conn,
        cm_db::repo::licenses::Source::CslbMasterList,
        "search_quality.csv",
        &[11u8; 32],
        None,
    )
    .await
    .expect("run");

    let trades = cm_db::repo::reference::all_trades(&mut conn)
        .await
        .expect("trades");

    for seed in CORPUS {
        let record = cm_db::repo::licenses::LicenseRecord {
            license_no: seed.license_no.to_owned(),
            business_name: seed.name.to_owned(),
            business_type: Some("Corporation".to_owned()),
            status: cm_db::repo::licenses::LicenseStatus::parse(seed.status).expect("status"),
            status_raw: seed.status.to_uppercase(),
            issue_date: None,
            expiration_date: None,
            classifications: vec![seed.classification.to_owned()],
            bond_amount_cents: Some(2_500_000),
            workers_comp_status: Some("Covered".to_owned()),
            address_line1: Some("100 Main St".to_owned()),
            city: Some("Los Angeles".to_owned()),
            state: Some("CA".to_owned()),
            postal_code: Some(seed.postal_code.to_owned()),
            county: Some("LOS ANGELES".to_owned()),
            phone: None,
            raw: serde_json::json!({ "LicenseNo": seed.license_no }),
            content_hash: vec![0u8; 32],
        };

        let stored = cm_db::repo::licenses::upsert(&mut conn, run_id, &record)
            .await
            .expect("licence");

        let region = cm_db::repo::reference::find_zcta(&mut conn, seed.postal_code)
            .await
            .expect("zcta")
            .expect("known zip");

        let upserted = cm_db::repo::contractors::upsert_from_license(
            &mut conn,
            &cm_db::repo::contractors::SourceFacts {
                license_record_id: stored.id,
                display_name: seed.name.to_owned(),
                slug: format!("{}-{}", cm_domain::slugify(seed.name), seed.license_no),
                postal_code: Some(seed.postal_code.to_owned()),
                region_id: Some(region.id),
            },
        )
        .await
        .expect("contractor");

        // Only the six canonical classifications resolve. C-20 and C-46 fall
        // through to an empty vec — deliberately; see the corpus note.
        let trade_ids: Vec<uuid::Uuid> = trades
            .iter()
            .filter(|trade| trade.cslb_classification.as_deref() == Some(seed.classification))
            .map(|trade| trade.id)
            .collect();
        cm_db::repo::contractors::replace_cslb_trades(&mut conn, upserted.id, &trade_ids)
            .await
            .expect("trades");

        cm_domain::location::republish(&mut conn, upserted.id)
            .await
            .expect("locate");
        cm_domain::verification::recompute(&mut conn, upserted.id, Some(run_id))
            .await
            .expect("verify");

        // The reputation and trust signals a blended ranking will read. Written
        // directly because their real sources — the Google enrichment load and
        // an approved claim — are not what this test measures.
        //
        // `verified_at` moves with `verified`: a CHECK constraint refuses a
        // badge with no date on it, which is the schema declining to store a
        // claim it cannot say the age of.
        sqlx::query(
            "UPDATE contractors \
                SET bio = $2, google_rating = $3, google_review_count = $4, \
                    verified = $5, \
                    verified_at = CASE WHEN $5 THEN now() ELSE NULL END \
              WHERE id = $1",
        )
        .bind(upserted.id)
        .bind(seed.bio)
        .bind(seed.rating)
        .bind(seed.reviews)
        .bind(seed.verified)
        .execute(&mut *conn)
        .await
        .expect("signals");
    }

    drop(conn);
    score_the_corpus(pool).await;
}

/// Derive the standing quality scores the ranking orders by.
///
/// The seeder writes ratings, review counts and badges directly; this turns
/// them into the single number `sort=best` reads. Without it every listing
/// scores zero, the blend collapses to text relevance alone, and the gate would
/// measure a search with its ranking switched off.
async fn score_the_corpus(pool: &PgPool) {
    cm_domain::quality::recompute_all(pool, &cm_domain::quality::Weights::default())
        .await
        .expect("quality scores");
}

/// Licence number by contractor id, so a ranked page can be scored against
/// judgements written in terms of licences.
async fn licence_by_id(pool: &PgPool) -> HashMap<uuid::Uuid, String> {
    sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT c.id, l.license_no \
           FROM contractors c JOIN license_records l ON l.id = c.license_record_id",
    )
    .fetch_all(pool)
    .await
    .expect("licences")
    .into_iter()
    .collect()
}

/* ── The report ────────────────────────────────────────────────────────────*/

struct Scored {
    query: String,
    intent: String,
    ndcg: f64,
    recall: f64,
    returned: usize,
}

async fn score_golden_set(pool: &PgPool) -> Vec<Scored> {
    let licences = licence_by_id(pool).await;
    let mut conn = search_connection(pool).await;
    let mut scored = Vec::new();

    for entry in golden_set() {
        // Routed through the alias vocabulary first, the way the handler does
        // it. Skipping that here would measure a search the product does not
        // ship.
        let query_trade_ids = cm_db::repo::reference::trades_matching_text(&mut conn, &entry.query)
            .await
            .expect("trade routing");

        let filters = Filters {
            query: Some(entry.query.clone()),
            query_trade_ids,
            ..Filters::default()
        };

        // Ranked exactly as a visitor gets them: the default sort, a full page.
        let page = search::list(&mut conn, &filters, Sort::Best, search::MAX_PAGE, None)
            .await
            .expect("search");

        let returned: Vec<String> = page
            .contractors
            .iter()
            .map(|found| licences.get(&found.id).cloned().unwrap_or_default())
            .collect();

        scored.push(Scored {
            ndcg: ndcg_at(&returned, &entry.judgements, 10),
            recall: recall_at(&returned, &entry.judgements, 20),
            returned: returned.len(),
            query: entry.query,
            intent: entry.intent,
        });
    }

    scored
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let collected: Vec<f64> = values.collect();
    if collected.is_empty() {
        return 0.0;
    }
    collected.iter().sum::<f64>() / collected.len() as f64
}

fn report(scored: &[Scored]) {
    println!("\n  query                  ndcg@10  recall@20  rows  intent");
    println!("  {}", "-".repeat(96));
    for entry in scored {
        println!(
            "  {:<22} {:>7.3}  {:>9.3}  {:>4}  {}",
            entry.query, entry.ndcg, entry.recall, entry.returned, entry.intent
        );
    }
    println!("  {}", "-".repeat(96));
    println!(
        "  {:<22} {:>7.3}  {:>9.3}\n",
        "mean",
        mean(scored.iter().map(|entry| entry.ndcg)),
        mean(scored.iter().map(|entry| entry.recall)),
    );
}

/* ── The gate ──────────────────────────────────────────────────────────────*/

/// The gate every ranking change has to pass.
#[sqlx::test(migrations = "../../migrations")]
async fn search_relevance_does_not_regress(pool: PgPool) {
    seed_corpus(&pool).await;
    let scored = score_golden_set(&pool).await;
    report(&scored);

    let ndcg = mean(scored.iter().map(|entry| entry.ndcg));
    let recall = mean(scored.iter().map(|entry| entry.recall));

    assert!(
        ndcg >= NDCG_FLOOR,
        "mean NDCG@10 fell to {ndcg:.3}, below the pinned floor of {NDCG_FLOOR:.3}. \
         Run with --nocapture for the per-query table."
    );
    assert!(
        recall >= RECALL_FLOOR,
        "mean Recall@20 fell to {recall:.3}, below the pinned floor of {RECALL_FLOOR:.3}. \
         Run with --nocapture for the per-query table."
    );
}

/// Plain name lookup is what search already does well, and what a
/// recall-chasing change is most likely to break without anyone noticing — the
/// mean can absorb it. This pins it separately.
#[sqlx::test(migrations = "../../migrations")]
async fn looking_a_business_up_by_name_stays_excellent(pool: PgPool) {
    seed_corpus(&pool).await;
    let scored = score_golden_set(&pool).await;

    let by_name: Vec<&Scored> = scored
        .iter()
        .filter(|entry| NAME_QUERIES.contains(&entry.query.as_str()))
        .collect();

    assert_eq!(
        by_name.len(),
        NAME_QUERIES.len(),
        "a query named in NAME_QUERIES is missing from the golden set"
    );

    for entry in &by_name {
        println!("  {:<22} ndcg@10 {:.3}", entry.query, entry.ndcg);
    }

    let ndcg = mean(by_name.iter().map(|entry| entry.ndcg));
    assert!(
        ndcg >= NAME_QUERY_NDCG_FLOOR,
        "name lookup fell to {ndcg:.3}, below {NAME_QUERY_NDCG_FLOOR:.3}: {:?}",
        by_name
            .iter()
            .map(|entry| (&entry.query, entry.ndcg))
            .collect::<Vec<_>>()
    );
}

/// A search for something nobody offers must return nothing. It is the cheapest
/// check that a relevance change has not made the predicate match everything —
/// which is what a mis-set fuzzy threshold does, and it raises no error
/// anywhere else.
#[sqlx::test(migrations = "../../migrations")]
async fn a_query_matching_nothing_still_returns_nothing(pool: PgPool) {
    seed_corpus(&pool).await;
    let mut conn = search_connection(&pool).await;

    let page = search::list(
        &mut conn,
        &Filters {
            query: Some("zzzzznotarealbusiness".to_owned()),
            ..Filters::default()
        },
        Sort::Best,
        search::MAX_PAGE,
        None,
    )
    .await
    .expect("search");

    assert!(
        page.contractors.is_empty(),
        "expected no matches, got {:?}",
        page.contractors
            .iter()
            .map(|found| &found.display_name)
            .collect::<Vec<_>>()
    );
}

/* ── Metric self-checks ────────────────────────────────────────────────────
 * A metric that is wrong reports a healthy number for an unhealthy search, so
 * the scorers are checked against orderings whose answers are known by hand.
 */

#[test]
fn ndcg_rewards_the_better_ordering() {
    let judgements: HashMap<String, f64> = [("a".to_owned(), 3.0), ("b".to_owned(), 1.0)]
        .into_iter()
        .collect();

    let ideal = vec!["a".to_owned(), "b".to_owned()];
    let swapped = vec!["b".to_owned(), "a".to_owned()];
    let padded = vec!["x".to_owned(), "a".to_owned(), "b".to_owned()];

    assert!((ndcg_at(&ideal, &judgements, 10) - 1.0).abs() < 1e-9);
    assert!(ndcg_at(&swapped, &judgements, 10) < 1.0);
    assert!(
        ndcg_at(&padded, &judgements, 10) < ndcg_at(&ideal, &judgements, 10),
        "an irrelevant result in first place must cost something"
    );
    assert_eq!(ndcg_at(&[], &judgements, 10), 0.0);
}

#[test]
fn recall_counts_what_was_found_not_where() {
    let judgements: HashMap<String, f64> = [
        ("a".to_owned(), 3.0),
        ("b".to_owned(), 1.0),
        ("c".to_owned(), 2.0),
    ]
    .into_iter()
    .collect();

    assert_eq!(recall_at(&["a".to_owned()], &judgements, 20), 1.0 / 3.0);
    assert_eq!(
        recall_at(
            &["c".to_owned(), "b".to_owned(), "a".to_owned()],
            &judgements,
            20
        ),
        1.0
    );

    // Beyond k does not count.
    let mut deep: Vec<String> = (0..25).map(|index| format!("filler{index}")).collect();
    deep.push("a".to_owned());
    assert_eq!(recall_at(&deep, &judgements, 20), 0.0);
}

/// An empty judgement set means "this query should find nothing".
#[test]
fn a_query_with_no_right_answer_is_scored_on_returning_nothing() {
    let none: HashMap<String, f64> = HashMap::new();

    assert_eq!(ndcg_at(&[], &none, 10), 1.0);
    assert_eq!(recall_at(&[], &none, 20), 1.0);
    assert_eq!(recall_at(&["anything".to_owned()], &none, 20), 0.0);
}

/// The corpus and the golden set have to talk about the same businesses. A
/// judgement naming a licence that is not seeded scores zero forever and reads
/// like a relevance bug.
#[test]
fn every_judgement_names_a_licence_in_the_corpus() {
    let seeded: Vec<&str> = CORPUS.iter().map(|seed| seed.license_no).collect();

    for entry in golden_set() {
        for licence in entry.judgements.keys() {
            assert!(
                seeded.contains(&licence.as_str()),
                "golden query {:?} judges licence {licence}, which the corpus does not seed",
                entry.query
            );
        }
    }
}
