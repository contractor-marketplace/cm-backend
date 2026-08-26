-- 0020 · Where a contractor's supporting data came from.
--
-- Added for the Google Maps enrichment load, which needs to distinguish a
-- contractor carrying real scraped reviews from one that will be given
-- generated pricing and review data for the demo. Without the distinction the
-- two are indistinguishable in the database, and the first person to ask "are
-- these reviews real?" has no way to find out.
--
-- This is a normal migration rather than an ALTER run by the enrichment tool,
-- even though the tool creates the rest of its own tables. `contractors` is
-- product schema: a column added outside the migration system would exist on
-- this deployment and on no fresh one, and `serve` would happily start against
-- a schema that silently differs from the migrations that claim to describe it.
--
-- The enrichment tool's own tables are the opposite case and live in a
-- `staging` schema, outside the invariants this suite enforces on `public`.
-- See tools/gmaps-enrichment/store.py for why.

ALTER TABLE contractors
    ADD COLUMN data_source text NOT NULL DEFAULT 'cslb'
        CHECK (data_source IN ('cslb', 'google', 'synthetic'));

-- Every existing row came from the CSLB import, which the default already
-- says. Stated rather than assumed, because the default only governs rows
-- written after this point.
UPDATE contractors SET data_source = 'cslb' WHERE data_source IS DISTINCT FROM 'cslb';

-- The default stays, unlike the pattern in 0018 where new columns had theirs
-- dropped after backfill. There the default existed only to make the backfill
-- legal and leaving it would have let a forgotten field write a silent
-- 'unsure'. Here 'cslb' is the genuine default for a row created by the
-- importer, which is the only thing that creates contractors today.

-- Partial: 'cslb' is essentially the whole table and indexing it would be
-- pointless, while the enriched minority is exactly what gets queried.
CREATE INDEX contractors_data_source_idx ON contractors (data_source)
    WHERE data_source <> 'cslb';
