-- 0019 · The contractor directory publishes the address on the licence.
--
-- 0010 built this table around a rule: a listing publishes its ZIP centroid
-- unless a *claimant* opted into publishing their exact address. That rule was
-- written for a marketplace where the address was ours to protect. It is not.
--
-- Every listing here comes from the CSLB public licence register, which
-- publishes each licensee's business address as a matter of law. Anyone can look
-- up any of these addresses in seconds. Plotting a ZIP centroid instead did not
-- conceal anything; it just made the directory worse at the one thing a person
-- uses it for — finding somebody near them.
--
-- So the default flips. A listing publishes the address the register already
-- publishes.
--
-- WHAT DOES NOT CHANGE, and is the reason this is safe to do:
--
-- Search, map and detail all read `public_point` and none of them reads
-- `precise_point`. That is unchanged here — this migration changes what
-- `public_point` CONTAINS, not which column anything reads. The invariant that
-- mattered in 0010 was never "the published point is coarse", it was "search
-- reads the same point the map shows", so a radius filter cannot be
-- binary-searched to recover a point the map was rounding off. That still holds
-- exactly as before, and it holds trivially now: there is nothing finer behind
-- the published point to recover.
--
-- `address_visibility` survives as a column, and 'protected' survives as a
-- value. Nothing sets it today, but it is where a takedown request lands — see
-- issue #9 — and `location::republish` still honours it. Deleting it would mean
-- rebuilding the mechanism the first time somebody asks to come off the map.
--
-- Jobs are the opposite case and are deliberately untouched: `jobs` has no
-- address column at all, because a homeowner's address was never published by
-- anybody and is not ours to start publishing.

-- An unclaimed listing may now publish the address, because the address was
-- already public before we imported it. This is the constraint 0010 added to
-- stop exactly that, and it is the one thing standing between the register and
-- the map.
ALTER TABLE contractors DROP CONSTRAINT contractors_only_claimed_may_publish_address;

ALTER TABLE contractors ALTER COLUMN address_visibility SET DEFAULT 'public';

UPDATE contractors SET address_visibility = 'public';

-- Republish. `precise_point` is already populated for the listings the US Census
-- geocoder resolved; this promotes it to the published point.
--
-- Everything else keeps whatever it had. A listing the geocoder could not place
-- keeps its ZIP centroid, and one with no known ZIP stays unlocated — better an
-- honest gap than a pin somewhere plausible. `public_point_source` is what lets
-- a client say which of the three it is looking at, which is why it is set here
-- rather than inferred from the coordinates.
UPDATE contractors
   SET public_point = precise_point,
       public_point_source = 'exact',
       updated_at = now()
 WHERE precise_point IS NOT NULL
   AND address_visibility = 'public';
