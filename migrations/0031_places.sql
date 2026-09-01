-- 0031 · Places, from the Census rather than from memory.
--
-- A ZCTA is a postal delivery geography. It is not a city, it does not nest
-- inside one, and it has no name of its own — the Census calls 90026 "90026".
-- Somewhere upstream that got confused, and `regions.name` was filled from a
-- file written by hand: 326 rows, none of them joined to anything. Four ZCTAs
-- came out named "Glendora", two of which are Rancho Cucamonga and Ontario.
-- 92105 came out "Glendale" and is in San Diego, ninety miles from the one it
-- claims to be.
--
-- The damage is not the wrong labels, which are merely wrong. It is that the
-- product had no way to tell two places apart: `?q=Glen` returned two rows both
-- reading "Glendora, 2 ZIP codes", 24 km apart, because a place here was a name
-- and a point with nothing above it. `repo/suggest.rs` grew an ST_ClusterDBSCAN
-- window function to stop a San Diego namesake dragging Glendale's centre into
-- open ground — a distance heuristic standing in for a hierarchy.
--
-- So: places become rows with a county parent, membership becomes a weighted
-- relationship the Census computed, and a ZCTA goes back to being called by its
-- code. Every column this needs already exists — `kind` has permitted 'city'
-- and 'county' since 0002, `parent_id` has been null on every row since 0002,
-- and `boundary` is an empty geography column waiting for a later pass.

-- A name a ZCTA never had. The city a ZIP sits in is a relationship, recorded
-- below, not a label on the postal geography.
--
-- This also clears the 25 curated Los Angeles neighbourhood names, which are
-- correct and which the Census cannot replace — Silver Lake is not a Census
-- place. They are restored by re-running `load-regions` against
-- deploy/data/zcta_la_county.csv after this migration; `upsert_zcta` keeps a
-- real name against a bare one, so the order matters and the runbook says so.
UPDATE regions SET name = code, updated_at = now()
 WHERE kind = 'zcta' AND name <> code;

-- Which ZIPs a place contains, and how much of each.
--
-- Weighted, because touching is not belonging. Burbank shares 0.01 km2 with
-- 90068 — a sliver where two boundaries graze in the Hollywood Hills, 0.0% of
-- either — and a plain "do these intersect" test would put a Hollywood ZIP in
-- Burbank. The Census publishes the shared land area for every pair, so the
-- loader can hold membership to a threshold and this table can carry the
-- evidence for the decision rather than just its outcome.
--
-- Both columns point at `regions`: a ZCTA on one side, a city on the other.
-- Named for the direction it is read in — given a ZIP, which places is it in.
CREATE TABLE region_places (
    id             uuid PRIMARY KEY,
    region_id      uuid NOT NULL REFERENCES regions (id) ON DELETE CASCADE,
    place_id       uuid NOT NULL REFERENCES regions (id) ON DELETE CASCADE,
    -- Square metres the two share, straight from the Census relationship file.
    -- Big enough for a county-sized place, so bigint rather than integer.
    shared_land_m2 bigint NOT NULL CHECK (shared_land_m2 >= 0),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT region_places_pair_key UNIQUE (region_id, place_id),
    -- A place is not inside itself.
    CONSTRAINT region_places_distinct CHECK (region_id <> place_id)
);

-- `region_places_pair_key` leads with region_id and supports that foreign key.
-- place_id needs its own, or deleting a place turns into a sequential scan
-- while holding the lock.
CREATE INDEX region_places_place_idx ON region_places (place_id);

COMMENT ON TABLE region_places IS
    'ZCTA-to-place membership from the Census 2020 relationship file, weighted '
    'by shared land area so a boundary sliver can be told from real membership.';

-- How many contractors are actually in a region.
--
-- The place index is statewide because the ZCTA load already is, which means a
-- prefix like "san" matches San Francisco, San Jose and San Diego — none of
-- which this directory serves. Ordering suggestions by supply is what makes a
-- statewide index safe on a Los Angeles corpus: the places somebody can
-- actually get a contractor from come first, and the empty ones sink.
--
-- Denormalised on purpose, and recomputed rather than triggered — the same
-- treatment `contractors.quality_score` gets, on the same nightly pass. A
-- count that is a few hours stale ranks a suggestion list perfectly well.
ALTER TABLE regions ADD COLUMN contractor_count integer NOT NULL DEFAULT 0
    CHECK (contractor_count >= 0);

COMMENT ON COLUMN regions.contractor_count IS
    'Listings whose postal code falls in this region. Refreshed by load-places '
    'and by recompute-verification; ranks suggestions by supply. See 0031.';

-- Somewhere to hold the Census place class, so a legally incorporated city
-- (C1) can be told from a statistical census-designated place (U1). California
-- has 482 of the first and 1,059 of the second, and only the first has a legal
-- boundary anybody filed. A later pass that draws boundaries needs to know
-- which of the two it is looking at before it asserts a line on a map.
ALTER TABLE regions ADD COLUMN census_class text
    CHECK (census_class IS NULL OR census_class ~ '^[A-Z][0-9]$');

COMMENT ON COLUMN regions.census_class IS
    'Census CLASSFP for a place row: C* legally incorporated, U* census '
    'designated. Null for ZCTAs and counties. See 0031.';
