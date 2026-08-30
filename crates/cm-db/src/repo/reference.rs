//! Reference data: trades and regions.

use cm_core::{new_id, AppError};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Trade {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub cslb_classification: Option<String>,
}

/// One CSLB classification, and whether the directory offers it as a filter.
pub struct CanonicalTrade {
    pub slug: &'static str,
    pub name: &'static str,
    pub classification: &'static str,
    /// Shown in the trade picker. Everything here is matched on import
    /// regardless — `featured` is only about what a homeowner is offered, and
    /// a list of 75 that opens with "Air and Water Balancing" is not a filter,
    /// it is a haystack.
    pub featured: bool,
}

/// The CSLB classification set.
///
/// Previously this held six entries, on the reasoning that the classification
/// list is long and v1 only filters on a handful. The cost was invisible and
/// large: `import.rs` maps a licence to a trade through this table and drops
/// what it cannot match, so a licence in any other class arrived carrying **no
/// trade at all**. Measured against the real register, six classifications
/// cover 61% of the 311,732 licence-classification pairs in it; a 3,000-row
/// sample left 27% of contractors matching no trade filter that exists.
///
/// These 75 cover 98.9%. What is left out is left out on purpose:
///
///   - `ASB` and `HAZ` are certifications, not classifications. A contractor
///     is not "a HAZ" the way they are a plumber.
///   - `C-49` and a handful of D-codes appear in the register but are not in
///     CSLB's current published list. They are named nowhere we can check, and
///     a guessed name would be a wrong label shown to a homeowner rather than
///     an absent one. They stay unmapped, and the importer now counts what it
///     drops instead of discarding it silently.
///
/// Order here is the order in the picker.
pub const CANONICAL_TRADES: &[CanonicalTrade] = &{
    macro_rules! trade {
        ($slug:literal, $name:literal, $class:literal, featured) => {
            CanonicalTrade {
                slug: $slug,
                name: $name,
                classification: $class,
                featured: true,
            }
        };
        ($slug:literal, $name:literal, $class:literal) => {
            CanonicalTrade {
                slug: $slug,
                name: $name,
                classification: $class,
                featured: false,
            }
        };
    }

    [
        // What a homeowner actually looks for, most-searched first.
        trade!("general-contractor", "General Contractor", "B", featured),
        trade!("electrician", "Electrician", "C-10", featured),
        trade!("plumber", "Plumber", "C-36", featured),
        trade!("hvac", "Heating & Air Conditioning", "C-20", featured),
        trade!("roofer", "Roofer", "C-39", featured),
        trade!("painter", "Painter", "C-33", featured),
        trade!("landscaper", "Landscaper", "C-27", featured),
        trade!("flooring", "Flooring", "C-15", featured),
        trade!("tile", "Tile & Stone", "C-54", featured),
        trade!("concrete", "Concrete", "C-8", featured),
        trade!("drywall", "Drywall", "C-9", featured),
        trade!(
            "finish-carpentry",
            "Cabinets & Finish Carpentry",
            "C-6",
            featured
        ),
        trade!("masonry", "Masonry", "C-29", featured),
        trade!("swimming-pool", "Swimming Pool", "C-53", featured),
        trade!("glazing", "Windows & Glass", "C-17", featured),
        trade!("solar", "Solar", "C-46", featured),
        trade!("tree-service", "Tree Service", "D-49", featured),
        trade!("fencing", "Fencing", "C-13", featured),
        trade!("low-voltage", "Low Voltage & Alarms", "C-7", featured),
        trade!("remodeling", "Residential Remodeling", "B-2", featured),
        trade!("insulation", "Insulation & Acoustical", "C-2", featured),
        trade!("sheet-metal", "Sheet Metal", "C-43", featured),
        trade!("plastering", "Lathing & Plastering", "C-35", featured),
        trade!("framing", "Framing & Rough Carpentry", "C-5", featured),
        trade!("siding", "Siding & Decking", "D-41", featured),
        trade!("doors-gates", "Doors & Gates", "D-28", featured),
        trade!("window-coverings", "Window Coverings", "D-52", featured),
        trade!(
            "pool-spa-service",
            "Pool & Spa Maintenance",
            "D-35",
            featured
        ),
        trade!(
            "demolition",
            "Building Moving & Demolition",
            "C-21",
            featured
        ),
        trade!("general-engineering", "General Engineering", "A", featured),
        // Matched on import, not offered as a filter.
        trade!("boiler", "Boiler, Hot Water Heating & Steam Fitting", "C-4"),
        trade!("elevator", "Elevator", "C-11"),
        trade!("earthwork-paving", "Earthwork & Paving", "C-12"),
        trade!("fire-protection", "Fire Protection", "C-16"),
        trade!("asbestos-abatement", "Asbestos Abatement", "C-22"),
        trade!("ornamental-metal", "Ornamental Metal", "C-23"),
        trade!("locksmith", "Lock & Security Equipment", "C-28"),
        trade!(
            "traffic-control",
            "Construction Zone Traffic Control",
            "C-31"
        ),
        trade!(
            "highway-improvement",
            "Parking & Highway Improvement",
            "C-32"
        ),
        trade!("pipeline", "Pipeline", "C-34"),
        trade!("refrigeration", "Refrigeration", "C-38"),
        trade!("sanitation-system", "Sanitation System", "C-42"),
        trade!("sign", "Sign", "C-45"),
        trade!(
            "manufactured-housing",
            "General Manufactured Housing",
            "C-47"
        ),
        trade!("reinforcing-steel", "Reinforcing Steel", "C-50"),
        trade!("structural-steel", "Structural Steel", "C-51"),
        trade!("water-conditioning", "Water Conditioning", "C-55"),
        trade!("well-drilling", "Well Drilling", "C-57"),
        trade!("welding", "Welding", "C-60"),
        trade!("limited-specialty", "Limited Specialty", "C-61"),
        trade!("awnings", "Awnings", "D-03"),
        trade!("central-vacuum", "Central Vacuum Systems", "D-04"),
        trade!("concrete-services", "Concrete Related Services", "D-06"),
        trade!(
            "drilling-blasting",
            "Drilling, Blasting & Oil Field Work",
            "D-09"
        ),
        trade!("elevated-floors", "Elevated Floors", "D-10"),
        trade!("synthetic-products", "Synthetic Products", "D-12"),
        trade!("hardware-safes", "Hardware, Locks & Safes", "D-16"),
        trade!("machinery-pumps", "Machinery & Pumps", "D-21"),
        trade!("metal-products", "Metal Products", "D-24"),
        trade!("paperhanging", "Paperhanging", "D-29"),
        trade!(
            "pile-driving",
            "Pile Driving & Pressure Foundation Jacking",
            "D-30"
        ),
        trade!(
            "pole-installation",
            "Pole Installation & Maintenance",
            "D-31"
        ),
        trade!("prefabricated-equipment", "Prefabricated Equipment", "D-34"),
        trade!("sand-water-blasting", "Sand & Water Blasting", "D-38"),
        trade!("scaffolding", "Scaffolding", "D-39"),
        trade!(
            "service-station-equipment",
            "Service Station Equipment & Maintenance",
            "D-40"
        ),
        trade!(
            "non-electrical-sign",
            "Non-Electrical Sign Installation",
            "D-42"
        ),
        trade!("suspended-ceilings", "Suspended Ceilings", "D-50"),
        trade!("wood-tanks", "Wood Tanks", "D-53"),
        trade!("trenching", "Trenching Only", "D-56"),
        trade!("hydroseed-spraying", "Hydroseed Spraying", "D-59"),
        trade!("air-water-balancing", "Air & Water Balancing", "D-62"),
        trade!("construction-cleanup", "Construction Clean-up", "D-63"),
        trade!("non-specialized", "Non-Specialized", "D-64"),
        trade!(
            "weatherization",
            "Weatherization & Energy Conservation",
            "D-65"
        ),
    ]
};

/// How close a query has to be to a curated alias before it routes to that
/// trade.
///
/// Higher than the 0.5 used for business names, and measured the same way. The
/// two are separated cleanly: correct matches score 0.737 and up
/// ("airconditioning" against "air conditioning" is 0.737, "water heaters"
/// against "water heater" 0.833), while wrong ones top out at 0.615 — "tree
/// removal" against "junk removal", which is exactly the mistake this rejects.
///
/// Typo tolerance is not lost by being strict here. A misspelled trade word
/// still reaches the right businesses through the name path: "plumer" scores
/// 0.571 against "Stillwater Plumbing", over the name threshold.
const ALIAS_SIMILARITY_THRESHOLD: f64 = 0.70;

/// The words homeowners use, and the trade each one means.
///
/// A person with a problem does not type a CSLB classification. They type
/// "water heater", "rewire", "adu", "hvac" — none of which appear in any
/// business name, which is why every one of them returned nothing however good
/// the taxonomy got. This is the layer between how a problem is described and
/// how a licence is classified.
///
/// A table rather than a model on purpose: the mapping is small and knowable,
/// and a wrong entry is fixed by editing one line rather than retraining
/// something. Matching is trigram, so near spellings reach the same row without
/// every variant being listed here.
///
/// Keyed by trade slug. An alias may appear under more than one trade — asking
/// for a remodel legitimately means both a general contractor and a
/// remodeller — and the search widens to both rather than guessing.
const TRADE_ALIASES: &[(&str, &[&str])] = &[
    (
        "plumber",
        &[
            "plumbing",
            "water heater",
            "tankless water heater",
            "drain",
            "drains",
            "clogged drain",
            "leak",
            "leaking pipe",
            "burst pipe",
            "pipe",
            "pipes",
            "repipe",
            "sewer",
            "sewer line",
            "faucet",
            "toilet",
            "garbage disposal",
            "sump pump",
            "gas line",
            "water line",
            "shower valve",
        ],
    ),
    (
        "electrician",
        &[
            "electric",
            "electrical",
            "rewire",
            "rewiring",
            "wiring",
            "panel upgrade",
            "electrical panel",
            "outlet",
            "outlets",
            "breaker",
            "breaker box",
            "ev charger",
            "car charger",
            "lighting",
            "recessed lighting",
            "generator",
            "ceiling fan",
        ],
    ),
    (
        "hvac",
        &[
            "heating",
            "air conditioning",
            "airconditioning",
            "air conditioner",
            "ac",
            "a/c",
            "furnace",
            "heat pump",
            "ductwork",
            "duct",
            "ducts",
            "mini split",
            "cooling",
            "ventilation",
            "thermostat",
            "central air",
        ],
    ),
    (
        "roofer",
        &[
            "roofing",
            "roof",
            "roof repair",
            "roof leak",
            "reroof",
            "shingles",
            "tile roof",
            "flat roof",
            "gutters",
            "rain gutters",
        ],
    ),
    (
        "painter",
        &[
            "painting",
            "paint",
            "interior painting",
            "exterior painting",
            "repaint",
            "house painting",
        ],
    ),
    (
        "landscaper",
        &[
            "landscaping",
            "landscape",
            "garden",
            "gardening",
            "lawn",
            "sprinklers",
            "irrigation",
            "hardscape",
            "sod",
            "yard",
            "artificial turf",
        ],
    ),
    (
        "general-contractor",
        &[
            "general contracting",
            "contractor",
            "remodel",
            "remodeling",
            "renovation",
            "renovate",
            "addition",
            "home addition",
            "adu",
            "accessory dwelling unit",
            "granny flat",
            "kitchen remodel",
            "bathroom remodel",
            "builder",
            "construction",
            "build",
        ],
    ),
    (
        "remodeling",
        &[
            "remodel",
            "remodeling",
            "home remodel",
            "residential remodel",
            "kitchen remodel",
            "bathroom remodel",
        ],
    ),
    (
        "flooring",
        &[
            "floors",
            "floor",
            "hardwood",
            "hardwood floors",
            "laminate",
            "carpet",
            "vinyl plank",
            "lvp",
            "floor refinishing",
            "subfloor",
        ],
    ),
    (
        "tile",
        &[
            "tiling",
            "tile work",
            "ceramic tile",
            "backsplash",
            "grout",
            "stone",
            "marble",
            "shower tile",
        ],
    ),
    (
        "concrete",
        &[
            "driveway",
            "foundation",
            "slab",
            "patio",
            "concrete work",
            "concrete driveway",
            "sidewalk",
            "footings",
        ],
    ),
    (
        "drywall",
        &[
            "sheetrock",
            "drywall repair",
            "plaster repair",
            "wall repair",
            "texture",
            "patch",
        ],
    ),
    (
        "finish-carpentry",
        &[
            "cabinets",
            "cabinetry",
            "kitchen cabinets",
            "millwork",
            "trim",
            "baseboards",
            "crown molding",
            "carpenter",
            "carpentry",
            "built ins",
            "closet",
        ],
    ),
    (
        "masonry",
        &[
            "brick",
            "brickwork",
            "block wall",
            "stone wall",
            "chimney",
            "pavers",
            "retaining wall",
        ],
    ),
    (
        "swimming-pool",
        &[
            "pool",
            "pool construction",
            "new pool",
            "spa",
            "hot tub",
            "pool remodel",
        ],
    ),
    (
        "pool-spa-service",
        &[
            "pool cleaning",
            "pool service",
            "pool maintenance",
            "spa maintenance",
        ],
    ),
    (
        "glazing",
        &[
            "windows",
            "window",
            "window replacement",
            "glass",
            "shower door",
            "mirror",
            "sliding door",
            "double pane",
        ],
    ),
    (
        "solar",
        &[
            "solar panels",
            "solar panel",
            "photovoltaic",
            "pv",
            "solar installation",
            "battery storage",
        ],
    ),
    (
        "tree-service",
        &[
            "tree",
            "tree removal",
            "tree trimming",
            "tree service",
            "arborist",
            "stump",
            "stump grinding",
        ],
    ),
    (
        "fencing",
        &[
            "fence",
            "fences",
            "fence repair",
            "wood fence",
            "vinyl fence",
        ],
    ),
    (
        "low-voltage",
        &[
            "alarm",
            "alarm system",
            "security system",
            "cameras",
            "security cameras",
            "cctv",
            "network",
            "data wiring",
            "intercom",
            "home theater",
            "structured wiring",
        ],
    ),
    (
        "insulation",
        &[
            "insulate",
            "insulation",
            "attic insulation",
            "soundproofing",
            "acoustical",
        ],
    ),
    (
        "sheet-metal",
        &[
            "ducting",
            "metal fabrication",
            "sheet metal work",
            "flashing",
        ],
    ),
    (
        "plastering",
        &["stucco", "lath", "plaster", "exterior plaster"],
    ),
    (
        "framing",
        &["framing", "rough carpentry", "framer", "structural framing"],
    ),
    (
        "siding",
        &[
            "siding",
            "deck",
            "decking",
            "deck repair",
            "siding repair",
            "trex",
        ],
    ),
    (
        "doors-gates",
        &[
            "door",
            "doors",
            "garage door",
            "gate",
            "automatic gate",
            "entry door",
        ],
    ),
    (
        "window-coverings",
        &["blinds", "shades", "shutters", "curtains", "drapes"],
    ),
    (
        "demolition",
        &[
            "demo",
            "demolition",
            "tear down",
            "house moving",
            "teardown",
        ],
    ),
    (
        "general-engineering",
        &[
            "grading",
            "excavation",
            "site work",
            "earthmoving",
            "engineering",
        ],
    ),
    (
        "earthwork-paving",
        &["paving", "asphalt", "resurfacing", "parking lot"],
    ),
    (
        "fire-protection",
        &[
            "fire sprinkler",
            "sprinkler system",
            "fire alarm",
            "fire protection",
        ],
    ),
    ("locksmith", &["locks", "locksmith", "rekey", "deadbolt"]),
    ("welding", &["welder", "welding", "metal work"]),
    (
        "refrigeration",
        &[
            "walk in cooler",
            "refrigeration",
            "commercial refrigeration",
        ],
    ),
    ("well-drilling", &["well", "water well", "well pump"]),
    (
        "water-conditioning",
        &[
            "water softener",
            "water filtration",
            "water treatment",
            "reverse osmosis",
        ],
    ),
    ("elevator", &["elevator", "lift", "stair lift"]),
    ("asbestos-abatement", &["asbestos", "asbestos removal"]),
    (
        "awnings",
        &["awning", "awnings", "patio cover", "shade structure"],
    ),
    ("central-vacuum", &["central vacuum", "vacuum system"]),
    ("scaffolding", &["scaffold", "scaffolding"]),
    (
        "construction-cleanup",
        &["construction cleanup", "debris removal", "junk removal"],
    ),
    (
        "weatherization",
        &["weatherization", "energy efficiency", "weatherproofing"],
    ),
    ("sign", &["sign", "signage", "signs"]),
];

/// Insert or refresh the canonical trade set. Idempotent.
///
/// Unlike its predecessor this updates on conflict rather than doing nothing.
/// `ON CONFLICT DO NOTHING` meant a database seeded once could never learn a
/// corrected name, a new classification mapping or a change to what the picker
/// offers — the constant would say one thing and every deployed database
/// another, with no way to tell from the outside.
///
/// The returned pair is (inserted-or-updated, total).
pub async fn seed_trades(conn: &mut PgConnection) -> Result<(u64, usize), AppError> {
    let mut written = 0;
    for (order, trade) in CANONICAL_TRADES.iter().enumerate() {
        let result = sqlx::query(
            "INSERT INTO trades (id, slug, name, cslb_classification, sort_order, active) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (slug) DO UPDATE \
                 SET name = EXCLUDED.name, \
                     cslb_classification = EXCLUDED.cslb_classification, \
                     sort_order = EXCLUDED.sort_order, \
                     active = EXCLUDED.active, \
                     updated_at = now()",
        )
        .bind(new_id())
        .bind(trade.slug)
        .bind(trade.name)
        .bind(trade.classification)
        .bind(order as i32)
        .bind(trade.featured)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;
        written += result.rows_affected();
    }

    Ok((written, CANONICAL_TRADES.len()))
}

/// Rewrite the alias vocabulary from `TRADE_ALIASES`.
///
/// Deleted and rebuilt rather than upserted, so removing an alias from the
/// constant actually removes it. The table is a projection of the constant and
/// nothing else writes to it; leaving stale rows behind would mean a mapping
/// somebody deliberately deleted kept routing searches.
///
/// Each trade's own name and slug are seeded as aliases too, so the lookup is
/// one query against one table instead of a union across three.
pub async fn seed_trade_aliases(conn: &mut PgConnection) -> Result<u64, AppError> {
    sqlx::query("DELETE FROM trade_aliases")
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;

    let mut written = 0;
    for trade in CANONICAL_TRADES {
        let extra = TRADE_ALIASES
            .iter()
            .find(|(slug, _)| *slug == trade.slug)
            .map(|(_, aliases)| *aliases)
            .unwrap_or(&[]);

        let own = [trade.name.to_lowercase(), trade.slug.replace('-', " ")];
        let aliases = own
            .iter()
            .map(String::as_str)
            .chain(extra.iter().copied())
            .map(|alias| alias.trim().to_lowercase())
            .filter(|alias| !alias.is_empty());

        for alias in aliases {
            let result = sqlx::query(
                "INSERT INTO trade_aliases (id, trade_id, alias) \
                 SELECT $1, t.id, $3 FROM trades t WHERE t.slug = $2 \
                 ON CONFLICT (trade_id, alias) DO NOTHING",
            )
            .bind(new_id())
            .bind(trade.slug)
            .bind(&alias)
            .execute(&mut *conn)
            .await
            .map_err(AppError::internal)?;
            written += result.rows_affected();
        }
    }

    Ok(written)
}

/// The trades a free-text query is asking for, if any.
///
/// This is the whole point of the alias table: "hvac" is not a business name
/// and never will be, so matching it against names and bios finds nothing no
/// matter how well that matching works. Resolving it to a trade first turns a
/// hopeless text query into an indexed semi-join.
///
/// Exact match first because it is the common case and needs no similarity
/// work; near matches second for the spellings nobody enumerated. Bounded,
/// because a query that routes to a dozen trades is not an intent, it is noise.
pub async fn trades_matching_text(
    conn: &mut PgConnection,
    query: &str,
) -> Result<Vec<Uuid>, AppError> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    // `word_similarity(...) >= threshold` rather than the `<%` operator, because
    // the operator reads the session threshold and that one is set for matching
    // business names. The two want different bars and this is not a preference:
    // at the name threshold of 0.5, "tree removal" matched the alias "junk
    // removal" at 0.615 and routed tree work to construction cleanup. A short
    // curated phrase is dominated by one shared common word in a way a business
    // name is not.
    //
    // The table is a few hundred rows, so scanning it costs nothing and no
    // index is load-bearing here.
    sqlx::query_scalar(
        "SELECT DISTINCT trade_id FROM trade_aliases \
          WHERE alias = $1 OR word_similarity($1, alias) >= $2 \
          LIMIT 8",
    )
    .bind(&needle)
    .bind(ALIAS_SIMILARITY_THRESHOLD)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Re-derive every contractor's CSLB trades from the licence already stored.
///
/// This has to exist, and it has to run after `seed_trades`, because neither a
/// migration nor a re-import can do it:
///
///   - A migration runs *before* `seed-trades` in the deploy order, so it would
///     derive against whatever taxonomy the previous release had — for the
///     change that grew six trades to seventy-five, that is a no-op.
///   - Re-importing the same CSLB file does nothing either: `import::flush`
///     short-circuits on `UpsertOutcome::Unchanged`, and an unchanged licence is
///     byte-identical by definition, so the trade-writing line is never reached.
///
/// Codes are matched the way the importer matches them — punctuation stripped,
/// upper-cased — because CSLB writes both `C10` and `C-6` in the same column of
/// the same file. Only `source = 'cslb'` rows are touched: a trade a claimant
/// self-reported is theirs, not ours to re-derive.
///
/// Returns (added, removed).
pub async fn rederive_cslb_trades(conn: &mut PgConnection) -> Result<(u64, u64), AppError> {
    const DERIVED: &str = "\
        SELECT DISTINCT c.id AS contractor_id, t.id AS trade_id \
          FROM contractors c \
          JOIN license_records l ON l.id = c.license_record_id \
          CROSS JOIN LATERAL unnest(l.classifications) AS raw(code) \
          JOIN trades t \
            ON upper(regexp_replace(t.cslb_classification, '[^A-Za-z0-9]', '', 'g')) \
             = upper(regexp_replace(raw.code, '[^A-Za-z0-9]', '', 'g')) \
         WHERE t.cslb_classification IS NOT NULL";

    let added = sqlx::query(&format!(
        "INSERT INTO contractor_trades (contractor_id, trade_id, source) \
         SELECT d.contractor_id, d.trade_id, 'cslb' FROM ({DERIVED}) d \
         ON CONFLICT (contractor_id, trade_id, source) DO NOTHING"
    ))
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?
    .rows_affected();

    // Drop CSLB rows the licence no longer supports, so a corrected mapping
    // cleans up after itself rather than leaving a contractor filed under a
    // trade the register stopped saying they hold.
    let removed = sqlx::query(&format!(
        "DELETE FROM contractor_trades ct \
          WHERE ct.source = 'cslb' \
            AND NOT EXISTS (SELECT 1 FROM ({DERIVED}) d \
                             WHERE d.contractor_id = ct.contractor_id \
                               AND d.trade_id = ct.trade_id)"
    ))
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?
    .rows_affected();

    Ok((added, removed))
}

/// The trades the directory offers as filters.
pub async fn all_trades(conn: &mut PgConnection) -> Result<Vec<Trade>, AppError> {
    sqlx::query_as(
        "SELECT id, slug, name, cslb_classification FROM trades \
          WHERE active ORDER BY sort_order, name",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

/// Every trade, filtered or not, for mapping a licence on import.
///
/// The importer must not use `all_trades`: that one hides anything unfeatured,
/// so a licence classified C-11 would import with no trade purely because the
/// directory does not offer "Elevator" in a dropdown. What a homeowner is
/// offered and what a licence can be classified as are different questions.
pub async fn all_trades_for_import(conn: &mut PgConnection) -> Result<Vec<Trade>, AppError> {
    sqlx::query_as("SELECT id, slug, name, cslb_classification FROM trades ORDER BY sort_order")
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Trade {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            slug: row.try_get("slug")?,
            name: row.try_get("name")?,
            cslb_classification: row.try_get("cslb_classification")?,
        })
    }
}

/// Trade ids for a set of slugs, for filtering.
pub async fn trade_ids_for_slugs(
    conn: &mut PgConnection,
    slugs: &[String],
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query_scalar("SELECT id FROM trades WHERE slug = ANY($1)")
        .bind(slugs)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::internal)
}

/// One ZIP-code area: its centroid is the published point for every contractor
/// whose exact address is protected.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Region {
    pub id: Uuid,
    pub kind: String,
    pub code: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

/// Insert or refresh a ZIP-code centroid.
///
/// A name equal to the code is a **placeholder**, and a placeholder never
/// overwrites a real name. The Census gazetteer — the only complete source of
/// ZIP centroids — publishes no names at all, so a bulk load carries the code
/// in that column; without this rule, loading it would replace "Silver Lake"
/// with "90026" for every ZIP anyone had bothered to name.
///
/// The centroid and source are always refreshed: those come from the file and
/// the file is the authority on them.
pub async fn upsert_zcta(
    conn: &mut PgConnection,
    code: &str,
    name: &str,
    lat: f64,
    lon: f64,
    source: &str,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "INSERT INTO regions (id, kind, code, name, centroid, source) \
         VALUES ($1, 'zcta', $2, $3, ST_SetSRID(ST_MakePoint($4, $5), 4326)::geography, $6) \
         ON CONFLICT (kind, code) DO UPDATE \
             SET name = CASE WHEN EXCLUDED.name = EXCLUDED.code \
                             THEN regions.name ELSE EXCLUDED.name END, \
                 centroid = EXCLUDED.centroid, \
                 source = EXCLUDED.source, updated_at = now()",
    )
    .bind(new_id())
    .bind(code)
    .bind(name)
    .bind(lon)
    .bind(lat)
    .bind(source)
    .execute(&mut *conn)
    .await
    .map_err(AppError::internal)?;

    Ok(result.rows_affected() == 1)
}

pub async fn find_zcta(conn: &mut PgConnection, code: &str) -> Result<Option<Region>, AppError> {
    // Never selects the geography itself: sqlx cannot decode PostGIS types, and
    // a query that returns one fails at the boundary.
    sqlx::query_as(
        "SELECT id, kind, code, name, ST_Y(centroid::geometry) AS lat, \
                ST_X(centroid::geometry) AS lon \
           FROM regions WHERE kind = 'zcta' AND code = $1",
    )
    .bind(code)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::internal)
}

pub async fn list_zctas(conn: &mut PgConnection) -> Result<Vec<Region>, AppError> {
    sqlx::query_as(
        "SELECT id, kind, code, name, ST_Y(centroid::geometry) AS lat, \
                ST_X(centroid::geometry) AS lon \
           FROM regions WHERE kind = 'zcta' ORDER BY code",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::internal)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Region {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            kind: row.try_get("kind")?,
            code: row.try_get("code")?,
            name: row.try_get("name")?,
            lat: row.try_get("lat")?,
            lon: row.try_get("lon")?,
        })
    }
}
