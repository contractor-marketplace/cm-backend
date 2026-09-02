-- 0026 · A standing quality score, so the directory can lead with the best.
--
-- The directory has always ordered alphabetically. `docs/architecture.md` open
-- item #2 records what that costs: 503 of 49,774 listings carry reviews, and
-- none of them appear on the first page, because the first page is whoever is
-- called "A...". Every signal needed to do better is already on the row —
-- google_rating, google_review_count, verified, claimed, and whether the
-- listing has been filled in at all — and none of them has ever influenced an
-- order.
--
-- Stored rather than computed per request, for two reasons. It is expensive to
-- recompute for 50,000 rows on every search, and more importantly a stored
-- column can be indexed, which is what keeps browsing keyset-paginated instead
-- of degrading to a sort of the whole table.
--
-- The value is derived, never authored: `recompute-verification` rewrites it on
-- the same nightly timer that re-derives the badge, from the same source data.
-- Nothing in the request path writes it, and no API accepts it.

ALTER TABLE contractors
    ADD COLUMN quality_score real NOT NULL DEFAULT 0
        CHECK (quality_score >= 0 AND quality_score <= 1);

-- The browse ordering, in full. The index carries the whole ORDER BY tuple —
-- score, then the stable key the cursor is built from — because a keyset scan
-- that has to sort its tail is not a keyset scan.
CREATE INDEX contractors_quality_idx
    ON contractors (quality_score DESC, display_name, id);
