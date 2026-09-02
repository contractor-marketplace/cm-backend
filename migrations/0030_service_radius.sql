-- 0030 · A travel radius is a property of a contractor, not a row in a list.
--
-- 0029 built service areas as one table with two kinds of row: a named region,
-- or a radius from the contractor's own point. That was right for the named
-- regions and wrong for the radius, and the difference is shape rather than
-- taste.
--
-- A contractor has exactly one travel radius, and always has one — even an
-- unclaimed listing, which is most of the directory. That is a column. Named
-- extra areas are a list of zero or more, which is a table. Storing both in the
-- table meant `num_nonnulls(region_id, radius_m) = 1` had to police a shape the
-- schema could otherwise have guaranteed, a contractor could hold four
-- contradictory travel radii, and — the part that actually mattered — there was
-- no way to express a default. `ServiceAreaEditor` already assumed one radius
-- (`areas.find(a => a.kind === "radius")`), so this makes the storage agree
-- with the interface that was written against it.
--
-- The default is the whole point. Search asks "who covers this address", and
-- until now a listing that had declared nothing answered "nowhere" — which is
-- every listing, since service areas are set by a claimant and the great
-- majority of the register is unclaimed. Giving every contractor 25 miles makes
-- the question answerable for the whole directory on the day it ships rather
-- than after claimants arrive.
--
-- 40234 metres is 25 miles. It is a stated product decision, not a derived
-- number: a licensed contractor is presumed willing to travel a normal
-- metropolitan distance unless they say otherwise. `DEFAULT_SERVICE_RADIUS_M`
-- in `repo/search.rs` is the same number, and
-- `the_default_radius_matches_the_schema` fails if the two drift.
ALTER TABLE contractors
    ADD COLUMN service_radius_m integer NOT NULL DEFAULT 40234
        CHECK (service_radius_m > 0 AND service_radius_m <= 200000);

COMMENT ON COLUMN contractors.service_radius_m IS
    'How far this contractor travels from public_point. Defaults to 25 miles '
    'for every listing, including unclaimed ones. See 0030.';

-- Carry over anything already declared. `max` rather than an arbitrary pick,
-- because the old CHECK permitted several radius rows per contractor and the
-- generous reading is the safe one: a contractor who said 10 and 50 miles is
-- more plausibly claiming the larger patch than being restricted to the
-- smaller. Empty in production — the table has never held a row — but dev and
-- test databases carry the feature's own fixtures.
UPDATE contractors c
   SET service_radius_m = sub.radius_m
  FROM (SELECT contractor_id, max(radius_m) AS radius_m
          FROM contractor_service_areas
         WHERE radius_m IS NOT NULL
         GROUP BY contractor_id) sub
 WHERE sub.contractor_id = c.id
   AND sub.radius_m BETWEEN 1 AND 200000;

DELETE FROM contractor_service_areas WHERE radius_m IS NOT NULL;

-- Dropping the column takes the two-kind CHECK and the partial index on
-- `radius_m` with it, which is the intent: the table now holds one kind of row
-- and the schema says so rather than a constraint policing it.
ALTER TABLE contractor_service_areas DROP COLUMN radius_m;
ALTER TABLE contractor_service_areas ALTER COLUMN region_id SET NOT NULL;

-- Coverage search asks a constant-radius question of the great majority and a
-- per-row question of the few who overrode it. The per-row half cannot use a
-- spatial index — that is the fault that cost 5× in 0029's predicate — so it is
-- resolved ahead of the statement, against this index, which spans only the
-- contractors who changed their radius. Partial on the same literal the column
-- defaults to; `the_custom_radius_index_matches_the_default` pins the two
-- together, because an index whose predicate drifts from the query's silently
-- stops being used rather than failing.
CREATE INDEX contractors_custom_radius_gix
    ON contractors USING gist (public_point)
    WHERE service_radius_m <> 40234;
