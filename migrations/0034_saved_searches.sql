-- 0034 · Saved searches, and the job-alert bookkeeping column.
--
-- Typed columns, not a JSON blob: the alert pass matches jobs against these
-- rows with the same clauses the live board's predicate uses, and a blob can
-- be neither indexed nor validated, nor kept honest when the board's filters
-- change — the migration test that cross-checks these CHECK lists against the
-- Rust enums is exactly the honesty a blob forfeits.
--
-- notify is the unsubscribe bit. Flipping it keeps the row: the person asked
-- to stop receiving email, not to lose the search they built.

CREATE TABLE saved_searches (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
    query text CHECK (length(query) <= 200),
    -- The query text's trade-alias expansion, frozen at save time by the same
    -- vocabulary the board routes through.
    query_trade_ids uuid[],
    trade_ids uuid[],                    -- NULL = any trade
    postal_code text CHECK (postal_code ~ '^[0-9]{5}$'),
    center geography (point, 4326),
    radius_m double precision CHECK (radius_m > 0 AND radius_m <= 200000),
    timeline text CHECK (timeline IN ('asap', 'within_2_weeks', 'more_than_2_weeks', 'unsure')),
    build_type text CHECK (build_type IN ('new_build', 'replacement', 'repair', 'unsure')),
    budget_min_cents bigint CHECK (budget_min_cents >= 0),
    notify boolean NOT NULL DEFAULT true,
    last_notified_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- A centre without a radius (or the reverse) is a half-filled filter, the
    -- same unrepresentable state the jobs budget CHECK forbids.
    CONSTRAINT saved_searches_near_is_complete CHECK ((center IS NULL) = (radius_m IS NULL))
);

CREATE INDEX saved_searches_user_idx ON saved_searches (user_id);

-- NULL = not yet matched against saved searches. Defaulting to NULL makes the
-- INSERT itself the enqueue — job posting needs no code change and no second
-- table. Pre-existing jobs are marked matched: what was on the board before
-- alerts existed never alerts.
ALTER TABLE jobs ADD COLUMN alerts_matched_at timestamptz;
UPDATE jobs SET alerts_matched_at = now();
CREATE INDEX jobs_alerts_pending_idx ON jobs (created_at)
    WHERE alerts_matched_at IS NULL;
