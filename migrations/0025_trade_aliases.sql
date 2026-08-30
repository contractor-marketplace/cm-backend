-- 0025 · The words homeowners use for trades.
--
-- Expanding the taxonomy made every CSLB classification reachable by the
-- `?trade=` filter. It did nothing for the search box, and the search box is
-- where people start: a query for "hvac" or "water heater" still matched
-- nothing, because free text is compared against a business name and a bio,
-- and no business is called "hvac".
--
-- The gap is vocabulary, not retrieval. A homeowner describes a problem —
-- "leaking pipe", "rewire", "adu" — and the register describes a licence class.
-- This table is the join between the two, and it is deliberately a table rather
-- than a model: the mapping is small, it is knowable, and an operator can
-- correct a wrong one in a single statement instead of retraining something.
--
-- Matching is trigram, not exact, so "airconditioning" and "air condition"
-- reach the same row as "air conditioning" without every spelling being
-- enumerated.

CREATE TABLE trade_aliases (
    id         uuid PRIMARY KEY,
    trade_id   uuid NOT NULL REFERENCES trades (id) ON DELETE CASCADE,
    -- Stored lower-case and trimmed; the seeder is the only writer.
    alias      text NOT NULL CHECK (btrim(alias) <> '' AND alias = lower(alias)),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    -- An alias may route to more than one trade — "remodel" is both a general
    -- contractor and a residential remodeller — but only once to each.
    CONSTRAINT trade_aliases_trade_alias_key UNIQUE (trade_id, alias)
);

-- Leads with the foreign key, per the schema invariant every FK is checked
-- against.
CREATE INDEX trade_aliases_trade_idx ON trade_aliases (trade_id);

-- Exact lookup, which is the common case and wants no similarity work at all.
CREATE INDEX trade_aliases_alias_idx ON trade_aliases (alias);

-- Fuzzy lookup, for the spellings nobody enumerated.
CREATE INDEX trade_aliases_alias_trgm ON trade_aliases USING GIN (alias gin_trgm_ops);
