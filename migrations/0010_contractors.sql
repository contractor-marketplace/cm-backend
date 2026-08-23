-- 0010 · Contractors: our record of a business, separate from both the CSLB
-- register and from accounts.
--
-- Location privacy lives here. `precise_point` is never selected by any API
-- read path; `public_point` is the only point search, map and detail use. That
-- is not merely a display rule: if distance search ran against the precise
-- point while the map showed a centroid, the radius filter could be
-- binary-searched to recover the address the centroid was protecting.

CREATE TABLE contractors (
    id uuid PRIMARY KEY,
    license_record_id uuid REFERENCES license_records (id) ON DELETE RESTRICT,
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND length(display_name) <= 200),
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9]+(-[a-z0-9]+)*$'),

    -- Claim state.
    claimed_by_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    claimed_at timestamptz,

    -- Claimant-managed profile. The importer never writes these.
    accepts_dm boolean NOT NULL DEFAULT false,
    bio text CHECK (length(bio) <= 2000),
    website_url text CHECK (length(website_url) <= 500),
    public_phone text CHECK (length(public_phone) <= 32),

    -- Location.
    address_visibility text NOT NULL DEFAULT 'protected'
        CHECK (address_visibility IN ('protected', 'public')),
    precise_point geography(Point, 4326),
    public_point geography(Point, 4326),
    public_point_source text NOT NULL DEFAULT 'none'
        CHECK (public_point_source IN ('exact', 'zip_centroid', 'none')),
    postal_code text CHECK (length(postal_code) <= 10),
    region_id uuid REFERENCES regions (id) ON DELETE SET NULL,

    -- Computed by exactly one function. No request payload reaches these.
    verified boolean NOT NULL DEFAULT false,
    verified_at timestamptz,
    verification_reason text CHECK (length(verification_reason) <= 500),

    search_doc tsvector GENERATED ALWAYS AS (
        to_tsvector(
            'public.english_unaccent',
            coalesce(display_name, '') || ' ' || coalesce(bio, '') || ' ' || coalesce(postal_code, '')
        )
    ) STORED,

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT contractors_license_key UNIQUE (license_record_id),
    CONSTRAINT contractors_slug_key UNIQUE (slug),
    -- NULLs do not collide, so this reads as "at most one claimed contractor
    -- per account" without forbidding many unclaimed ones.
    CONSTRAINT contractors_one_per_claimant UNIQUE (claimed_by_user_id),
    CONSTRAINT contractors_claim_is_complete
        CHECK ((claimed_by_user_id IS NULL) = (claimed_at IS NULL)),
    CONSTRAINT contractors_public_point_is_complete
        CHECK ((public_point IS NULL) = (public_point_source = 'none')),
    CONSTRAINT contractors_verified_is_dated
        CHECK (verified = false OR verified_at IS NOT NULL),
    -- An unclaimed listing can never be marked as publishing an exact address:
    -- nobody has asked for that address to be public.
    CONSTRAINT contractors_only_claimed_may_publish_address
        CHECK (address_visibility = 'protected' OR claimed_by_user_id IS NOT NULL)
);

CREATE INDEX contractors_public_point_gix ON contractors USING GIST (public_point)
    WHERE public_point IS NOT NULL;
CREATE INDEX contractors_search_doc_gin ON contractors USING GIN (search_doc);
CREATE INDEX contractors_name_trgm ON contractors USING GIN (display_name gin_trgm_ops);
CREATE INDEX contractors_verified_idx ON contractors (verified) WHERE verified;
CREATE INDEX contractors_keyset_idx ON contractors (display_name, id);
CREATE INDEX contractors_region_idx ON contractors (region_id);
CREATE INDEX contractors_claimant_idx ON contractors (claimed_by_user_id);
CREATE INDEX contractors_postal_code_idx ON contractors (postal_code);

CREATE TABLE contractor_trades (
    contractor_id uuid NOT NULL REFERENCES contractors (id) ON DELETE CASCADE,
    trade_id uuid NOT NULL REFERENCES trades (id) ON DELETE RESTRICT,
    -- A claimant self-reporting a trade CSLB already lists must not collide
    -- with the imported row, so provenance is part of the key.
    source text NOT NULL CHECK (source IN ('cslb', 'self_reported')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (contractor_id, trade_id, source)
);

CREATE INDEX contractor_trades_trade_idx ON contractor_trades (trade_id, contractor_id);

CREATE TABLE contractor_service_areas (
    id uuid PRIMARY KEY,
    contractor_id uuid NOT NULL REFERENCES contractors (id) ON DELETE CASCADE,
    region_id uuid REFERENCES regions (id) ON DELETE CASCADE,
    radius_m integer CHECK (radius_m > 0 AND radius_m <= 200000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    -- Exactly one kind of area per row: a named region, or a radius from the
    -- contractor's own point.
    CONSTRAINT contractor_service_areas_one_kind
        CHECK (num_nonnulls(region_id, radius_m) = 1),
    CONSTRAINT contractor_service_areas_unique_region UNIQUE (contractor_id, region_id)
);

CREATE INDEX contractor_service_areas_contractor_idx ON contractor_service_areas (contractor_id);
CREATE INDEX contractor_service_areas_region_idx ON contractor_service_areas (region_id);
