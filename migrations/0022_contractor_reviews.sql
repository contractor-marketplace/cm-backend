-- 0022 · Publishing the reviews the Google Maps enrichment load collected.
--
-- The enrichment tool writes to a `staging` schema that sits outside every
-- invariant this suite enforces, which was the right call for a scraper: its
-- tables are shaped by whatever an actor happens to return, and 0020 says so.
-- The consequence, though, was that ~25,000 real reviews existed in the
-- database with no way to reach the product. Nothing in `crates/` referenced
-- `staging` at all.
--
-- This migration is the bridge, and it is a COPY rather than a view.
--
-- A view in `public` selecting from `staging` was the obvious shortcut and is
-- wrong for one decisive reason: `migrations_apply_to_an_empty_database` runs
-- against a fresh database where the `staging` schema does not exist, because
-- nothing in the migration system creates it. `CREATE VIEW` resolves its
-- references at creation time, so the view would fail on every clean migrate
-- and every CI run. Duplicating the scraper's DDL here to satisfy that would
-- put the same tables in two places and invite exactly the drift the staging
-- split exists to avoid.
--
-- So published reviews are ordinary product tables that the invariants govern,
-- and a separate, re-runnable promotion step (tools/gmaps-enrichment/publish.sql)
-- carries rows across the boundary. That also puts the match-quality gate in
-- one reviewable place instead of hiding it in a view definition, and it means
-- the published set is stable: re-running the scraper cannot silently change
-- what the site shows until somebody promotes again.

-- ── Summary, denormalised onto the contractor ────────────────────────────────
--
-- On the contractor rather than in a joined summary table because the directory
-- list, the map and the profile all read the same projection in
-- crates/cm-db/src/repo/search.rs. A join there would be a fourth table in a
-- query that already carries three, to fetch two scalars.
--
-- All four are nullable and NULL means "not enriched", which is the honest
-- state for the ~49,000 contractors the load never reached.

ALTER TABLE contractors
    ADD COLUMN google_rating numeric(2, 1)
        CHECK (google_rating >= 1.0 AND google_rating <= 5.0),
    -- Google's own count across every review the place has, which is NOT the
    -- number of rows in contractor_reviews: the scrape caps at 200 per place
    -- and many places have more. The profile says "showing N of M" rather than
    -- pretending the sample is the whole.
    ADD COLUMN google_review_count integer
        CHECK (google_review_count >= 0),
    ADD COLUMN google_place_id text,
    ADD COLUMN google_reviews_synced_at timestamptz;

-- Partial, and mirroring contractors_data_source_idx from 0020: the enriched
-- minority is what gets queried, and indexing ~49,000 NULLs would be waste.
CREATE INDEX contractors_google_rating_idx ON contractors (google_rating DESC)
    WHERE google_rating IS NOT NULL;

-- ── The reviews themselves ───────────────────────────────────────────────────

CREATE TABLE contractor_reviews (
    id            uuid PRIMARY KEY,
    contractor_id uuid NOT NULL REFERENCES contractors (id) ON DELETE CASCADE,

    -- TEXT + CHECK rather than a native enum, per the convention the suite
    -- enforces. Pinned to ReviewSource::ALL in Rust by a test, so the two
    -- hand-written lists cannot drift.
    source        text NOT NULL CHECK (source IN ('google')),
    -- The provider's own identifier, so a re-promotion updates a review rather
    -- than duplicating it. Derived rather than given: the actor returns no
    -- review id, so the pipeline hashes the review's stable fields.
    external_id   text NOT NULL,

    author_name   text,
    rating        numeric(2, 1) NOT NULL CHECK (rating >= 1.0 AND rating <= 5.0),
    body          text,

    -- The age as Google phrased it — "a year ago" — and not a timestamp.
    --
    -- The actor never returned publishedAtDate, so staging.gmaps_reviews holds
    -- NULL in published_at for all 24,984 rows. Parsing "a year ago" into a
    -- date would invent a precision the source does not have, and sorting by it
    -- would produce an order that looks chronological and is not. Storing the
    -- phrase keeps the claim exactly as strong as the evidence.
    relative_age  text,

    owner_reply   text,
    photo_count   integer NOT NULL DEFAULT 0 CHECK (photo_count >= 0),

    -- Google's own ordering (its "most relevant" first), which is the only
    -- ordering available with no dates. 1-based, as the scraper recorded it.
    position      integer NOT NULL CHECK (position >= 1),

    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),

    -- Scoped to the contractor, not global to the source. 34 Google places are
    -- matched to more than one licence — usually one business holding several,
    -- sometimes a bad match — so the same review legitimately lands on more
    -- than one profile. A bare UNIQUE (source, external_id) would reject the
    -- second one and silently truncate those contractors to nothing.
    UNIQUE (contractor_id, source, external_id)
);

-- The foreign key needs an index leading with its column, and this is also the
-- access path the profile page uses: every review for one contractor, in
-- Google's order.
CREATE INDEX contractor_reviews_contractor_idx
    ON contractor_reviews (contractor_id, position);
