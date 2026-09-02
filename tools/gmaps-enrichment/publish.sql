-- Promote scraped Google reviews from `staging` into the product tables.
--
--   psql "$DATABASE_URL" -f tools/gmaps-enrichment/publish.sql
--
-- Idempotent and re-runnable. Run it again whenever the scraper has collected
-- more, or after changing the gate below; it recomputes the published set from
-- scratch and leaves the site consistent with whatever staging holds now.
--
-- Nothing else in the codebase reads `staging`. This file is the only crossing.
--
--
-- THE GATE, AND WHY IT IS NOT JUST "match_status = 'confirmed'"
--
-- The matcher's own confidence is not a good enough filter. Sampling the real
-- data after the load:
--
--   * `confirmed` is ~80% right overall, but its weakest band is not. Every one
--     of the twelve lowest-scoring confirmed matches was wrong — `nu way
--     plumbing` had been paired with `prodigy plumbing`, `electric automation`
--     with `network automation`. Precision collapses below a name similarity of
--     about 0.70 no matter which status the row carries.
--
--   * `needs_review` between 0.45 and 0.55 was ~11 of 12 wrong, and wrong in a
--     way that matters: `lanterprise` → `enterprise rent a car`, and two
--     physicians (`boris bagdasarian do`, `cynthia ro md`) whose patient reviews
--     would have been published on a contractor's profile.
--
-- The category signal that should have caught the physicians is unavailable:
-- the actor returned no placeCategory, so `place_category` is NULL on every
-- place and `category_plausible` scores 0.0 on every match. So the gate is
-- built from the two signals that do exist — how similar the names are, and
-- whether the Google business name reads like a trade at all.
--
-- WHERE THE FLOOR ACTUALLY BELONGS, measured on the published set
--
-- A first pass set this at 0.55 on the theory that precision degraded smoothly
-- and only collapsed below ~0.70. Scoring a random sample of 30 actually
-- published matches killed that theory: it came out 15 right, 15 wrong. The
-- degradation is not smooth, and 0.55 was not "a bit noisier" — it was a coin
-- flip.
--
-- The reason is visible once the errors are read together. Almost every one is
-- an initialism landing on a different initialism, which the similarity metric
-- scores generously because so few characters are in play:
--
--   W P CONSTRUCTION       → W F Construction              (0.94)
--   T L A HEATING & AIR    → VT Heating & Air Conditioning (0.90)
--   LBFC GENERAL CONSTR.   → DM Construction               (0.80)
--   ATOZ ELECTRIC INC      → AG Electric                   (0.77)
--   GARAY'S ELECTRIC       → Gary's Auto Electric          (0.70)
--
-- Those score high and are all wrong, so no floor below 1.00 separates them.
-- What does separate them is exactness: in that sample every match at 1.00 was
-- right and nearly everything under it was a toss-up, and a 20-row sample of
-- the 1.00 band on its own was clean.
--
-- Measured yields, in contractors published:
--
--   floor 0.55  →  821   (~50% wrong — roughly 410 contractors carrying
--                         someone else's reviews)
--   floor 0.75  →  697
--   floor 0.85  →  592
--   floor 0.90  →  555   (still admits the 0.90-0.94 initialism collisions)
--   floor 1.00  →  503   (exact normalised match; the sampled band was clean)
--
-- So: exact match only. It costs 318 contractors against the widest setting and
-- buys back the ~410 that would have been wrong, which is not a trade worth
-- deliberating over. Coverage was the stated priority for this launch and 503
-- is still real coverage — but "spread with reasonable accuracy" stops meaning
-- anything at a 50% error rate.
--
-- To widen it, lower this one number and re-run this file; that is the whole
-- migration path in either direction. Anything below 1.00 should come with a
-- fresh sample, because the band immediately under it is where the initialism
-- collisions live.

\set name_floor 1.00

-- A Google business name has to read like a trade. This is what excludes the
-- restaurants, physicians, law offices and car rentals that a surname match
-- otherwise drags in — CSLB lists sole proprietors by legal name, so `guerrero
-- federico jr` will match `guerrero taqueria` on the name alone.
--
-- Deliberately generous. A false negative costs one contractor's reviews; a
-- false positive publishes someone else's business on their profile.
\set trade_tokens '(construct|plumb|electric|roof|paint|hvac|heat|cool|cold|\\mair\\M|landscap|concrete|mason|remodel|build|contract|\\minc\\M|\\mllc\\M|\\mcorp\\M|compan|service|design|pool|steel|energy|solar|tile|floor|drywall|glass|door|window|fence|pest|clean|restor|engineer|develop|\\mhome|repair|install|mechanic|plaster|stucco|cabinet|granite|marble|asphalt|pav|excavat|demoli|insulat|septic|drill|sheet metal|weld|iron|fabricat|garage|awning|pipe|utilit|interior|kitchen|bath|deck|patio|carpent|handyman|maintenance|sewer|rooter|drain|water|refrigerat|boiler|furnace|duct|chimney|gutter|siding|proof|protect|\\mfire\\M|seal|coating|surfac|swim|crane|scaffold|equipment|works|tree|alarm|security|sign|upholst|screen|shutter|blind|spray|wash|haul|grading|environmental|abatement|lock|pump|tank|shoring|fram|lath|acoustic|terrazzo|ornamental|millwork|countertop|hardscape|irrigation|sprinkler|arborist|fumigat|termite|backflow|carpet|automation|properties|manufactur)'

BEGIN;

-- The set of matches that may be published, computed once so the three
-- statements below cannot disagree about who is in it.
CREATE TEMP TABLE published_match ON COMMIT DROP AS
SELECT m.contractor_id,
       m.place_id,
       p.overall_rating,
       p.total_reviews,
       p.place_url
  FROM staging.contractor_place_matches m
  JOIN staging.gmaps_places p USING (place_id)
 WHERE m.match_status IN ('confirmed', 'needs_review')
   AND (m.score_components ->> 'name_similarity')::numeric >= :name_floor
   AND m.score_components ->> 'normalised_place_name' ~ :'trade_tokens';

CREATE UNIQUE INDEX ON published_match (contractor_id);

-- ── Reviews ─────────────────────────────────────────────────────────────────
--
-- Delete first, so a contractor who fails a tightened gate loses their reviews
-- rather than keeping a stale set forever. Scoped to source = 'google' so a
-- future source is not collateral damage.
DELETE FROM contractor_reviews r
 WHERE r.source = 'google'
   AND NOT EXISTS (SELECT 1 FROM published_match pm
                    WHERE pm.contractor_id = r.contractor_id);

INSERT INTO contractor_reviews
    (id, contractor_id, source, external_id, author_name, rating, body,
     relative_age, owner_reply, photo_count, position)
SELECT gen_random_uuid(),
       pm.contractor_id,
       'google',
       v.review_id,
       nullif(btrim(v.reviewer_name), ''),
       v.rating,
       nullif(btrim(v.review_text), ''),
       nullif(btrim(v.published_at_raw), ''),
       nullif(btrim(v.owner_reply), ''),
       COALESCE(v.review_photo_count, 0),
       COALESCE(v.review_number, 1)
  FROM published_match pm
  JOIN staging.gmaps_reviews v ON v.place_id = pm.place_id
 -- A UUIDv4 rather than the UUIDv7 the application generates. Ordering here
 -- comes from `position`, never from the key, so the lost time ordering costs
 -- nothing — and a v7 would need either an extension or a round trip through
 -- Rust for a bulk copy that is otherwise one statement.
 ON CONFLICT (contractor_id, source, external_id) DO UPDATE
    SET author_name  = EXCLUDED.author_name,
        rating       = EXCLUDED.rating,
        body         = EXCLUDED.body,
        relative_age = EXCLUDED.relative_age,
        owner_reply  = EXCLUDED.owner_reply,
        photo_count  = EXCLUDED.photo_count,
        position     = EXCLUDED.position,
        updated_at   = now();

-- ── Summary on the contractor ───────────────────────────────────────────────
--
-- Cleared for everyone first, for the same reason the reviews are deleted
-- first: a contractor dropped by a tightened gate must lose the badge too.
UPDATE contractors
   SET google_rating            = NULL,
       google_review_count      = NULL,
       google_place_id          = NULL,
       google_place_url         = NULL,
       google_reviews_synced_at = NULL,
       data_source              = 'cslb',
       updated_at               = now()
 WHERE google_place_id IS NOT NULL
   AND id NOT IN (SELECT contractor_id FROM published_match);

UPDATE contractors c
   SET google_rating = pm.overall_rating,
       -- Google's total, not our row count: the scrape caps at 200 per place.
       -- COALESCE so a place that returned no total still gets an honest count
       -- from what we actually hold.
       google_review_count = COALESCE(
           pm.total_reviews,
           (SELECT count(*) FROM contractor_reviews r
             WHERE r.contractor_id = c.id AND r.source = 'google')),
       google_place_id          = pm.place_id,
       -- Stored verbatim as observed. See migrations/0023 for why this is not
       -- derived from the place id.
       google_place_url         = pm.place_url,
       google_reviews_synced_at = now(),
       -- The column 0020 added and nothing has used until now.
       data_source              = 'google',
       updated_at               = now()
  FROM published_match pm
 WHERE c.id = pm.contractor_id;

COMMIT;

-- ── What just happened ──────────────────────────────────────────────────────
\echo ''
\echo 'published:'
SELECT count(*) FILTER (WHERE google_place_id IS NOT NULL)              AS contractors,
       count(*) FILTER (WHERE google_rating IS NOT NULL)                AS with_a_rating,
       (SELECT count(*) FROM contractor_reviews WHERE source = 'google') AS reviews,
       (SELECT round(avg(google_rating), 2) FROM contractors
         WHERE google_rating IS NOT NULL)                               AS avg_rating
  FROM contractors;
