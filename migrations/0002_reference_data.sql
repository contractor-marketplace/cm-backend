-- 0002 · Reference data: geographic regions and the trade taxonomy.
--
-- Both tables are referenced by contractors (migration added in M3) and by the
-- geocoding fallback (M4). They land first because they have no dependency on
-- accounts, so M0 can prove the migration harness, the PostGIS types and the
-- GiST indexes without pulling any of the auth schema forward.
--
-- No rows are seeded here. ZCTA centroids arrive with the geocoding milestone
-- and the CSLB classification mapping arrives with the importer; seeding either
-- now would commit to a shape neither milestone has validated.

CREATE TABLE regions (
    id         uuid PRIMARY KEY,
    kind       text NOT NULL CHECK (kind IN ('zcta', 'county', 'city', 'neighborhood')),
    code       text NOT NULL CHECK (btrim(code) <> ''),
    name       text NOT NULL CHECK (btrim(name) <> ''),
    parent_id  uuid REFERENCES regions (id) ON DELETE SET NULL,
    -- geography, not geometry: distances come back in metres with no projection
    -- to choose, and ST_DWithin/<-> are index-assisted against a GiST index.
    centroid   geography(Point, 4326) NOT NULL,
    boundary   geography(MultiPolygon, 4326),
    source     text NOT NULL CHECK (btrim(source) <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT regions_kind_code_key UNIQUE (kind, code),
    CONSTRAINT regions_no_self_parent CHECK (parent_id IS NULL OR parent_id <> id)
);

CREATE INDEX regions_centroid_gix ON regions USING GIST (centroid);
-- Partial: most regions carry only a centroid, and indexing the NULLs is waste.
CREATE INDEX regions_boundary_gix ON regions USING GIST (boundary) WHERE boundary IS NOT NULL;
-- Supports the FK: an unindexed referencing column turns a parent delete into a
-- sequential scan while holding locks.
CREATE INDEX regions_parent_idx ON regions (parent_id);

CREATE TABLE trades (
    id                  uuid PRIMARY KEY,
    slug                text NOT NULL CHECK (slug ~ '^[a-z0-9]+(-[a-z0-9]+)*$'),
    name                text NOT NULL CHECK (btrim(name) <> ''),
    -- CSLB licence classification, e.g. 'B', 'C-10'. Left as a bounded free
    -- string rather than a pattern: the real classification set (A, B, B-2,
    -- C-2..C-61, D-xx) has not been validated against an actual CSLB file yet,
    -- and encoding a guess here would reject valid rows at import time.
    cslb_classification text CHECK (btrim(cslb_classification) <> '' AND length(cslb_classification) <= 16),
    sort_order          integer NOT NULL DEFAULT 0,
    active              boolean NOT NULL DEFAULT true,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT trades_slug_key UNIQUE (slug),
    CONSTRAINT trades_cslb_classification_key UNIQUE (cslb_classification)
);

CREATE INDEX trades_active_idx ON trades (sort_order, name) WHERE active;
