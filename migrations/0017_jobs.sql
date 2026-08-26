-- 0017 · Jobs: work a homeowner wants done, and contractors browse.
--
-- The other side of the marketplace. Contractors exist here because we imported
-- a public licence register; a job exists because a person wrote it, which makes
-- the privacy problem sharper rather than softer.
--
-- Note what this table does NOT have: a precise point, and an address of any
-- kind. Contractors carry `precise_point` because a claimant may opt into
-- publishing their own address. A homeowner is never offered that, so the
-- address is not collected at all — a column that does not exist cannot leak
-- through a handler that forgets to exclude it.
--
-- What is published is the ZIP centroid, exactly as for contractors, and the
-- radius filter reads that same published point. That is what stops the filter
-- being binary-searched to recover a location the centroid was protecting.

CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    posted_by_user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,

    -- The title is visible to everyone, including signed-out visitors; the
    -- description is not. That asymmetry is deliberate and the compose form
    -- says so, because free text is the one hole structural redaction cannot
    -- close: somebody who types their address into the title has published it.
    title text NOT NULL
        CHECK (btrim(title) <> '' AND length(title) <= 140),
    description text NOT NULL
        CHECK (btrim(description) <> '' AND length(description) <= 4000),

    -- RESTRICT, not CASCADE: deleting a trade must not silently delete the jobs
    -- filed under it. Nullable, because a homeowner may not know the trade.
    trade_id uuid REFERENCES trades (id) ON DELETE RESTRICT,

    status text NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'closed', 'cancelled')),

    -- Whole cents, so no floating point ever touches money. Both ends optional:
    -- "no idea" is a legitimate answer and better than an invented number.
    budget_min_cents bigint CHECK (budget_min_cents >= 0),
    budget_max_cents bigint CHECK (budget_max_cents >= 0),

    timeline text
        CHECK (timeline IN ('asap', 'within_a_month', 'within_three_months', 'flexible')),

    -- Location. The ZIP is the finest granularity this product stores.
    postal_code text CHECK (postal_code ~ '^[0-9]{5}$'),
    region_id uuid REFERENCES regions (id) ON DELETE SET NULL,
    public_point geography(Point, 4326),
    -- No 'exact': there is no path by which a job publishes a precise location.
    public_point_source text NOT NULL DEFAULT 'none'
        CHECK (public_point_source IN ('zip_centroid', 'none')),

    closed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    -- A point and its provenance are set together or not at all, so the pair can
    -- never be left half-applied by a partial write.
    CONSTRAINT jobs_public_point_is_complete
        CHECK ((public_point IS NULL) = (public_point_source = 'none')),

    CONSTRAINT jobs_budget_range_is_ordered
        CHECK (budget_min_cents IS NULL
               OR budget_max_cents IS NULL
               OR budget_min_cents <= budget_max_cents),

    CONSTRAINT jobs_closed_iff_not_open
        CHECK ((status = 'open') = (closed_at IS NULL))
);

CREATE INDEX jobs_posted_by_idx ON jobs (posted_by_user_id);
CREATE INDEX jobs_trade_idx ON jobs (trade_id);
CREATE INDEX jobs_region_idx ON jobs (region_id);

CREATE INDEX jobs_public_point_gix ON jobs USING GIST (public_point)
    WHERE public_point IS NOT NULL;

-- The board is newest-first over open jobs, and the keyset cursor is exactly
-- (created_at, id) — the same tuple this index is built on and the same tuple
-- the ORDER BY ends on. Keeping those three in agreement is what stops page two
-- from silently dropping rows.
CREATE INDEX jobs_board_idx ON jobs (created_at DESC, id DESC)
    WHERE status = 'open';

-- Only a homeowner account may post work, mirroring the rule that only a
-- contractor account may claim a listing. A trigger rather than a CHECK because
-- the condition spans two tables, and a CHECK cannot read users from jobs.
CREATE OR REPLACE FUNCTION jobs_poster_must_be_a_homeowner()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF (SELECT account_type FROM users WHERE id = NEW.posted_by_user_id) <> 'homeowner' THEN
        RAISE EXCEPTION
            'only a homeowner account may post a job (user %)',
            NEW.posted_by_user_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER jobs_poster_is_a_homeowner
    BEFORE INSERT OR UPDATE OF posted_by_user_id ON jobs
    FOR EACH ROW
    EXECUTE FUNCTION jobs_poster_must_be_a_homeowner();
