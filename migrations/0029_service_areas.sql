-- 0029 · Where a contractor actually works.
--
-- `contractor_service_areas` has existed since 0010 and has never been used.
-- Not "written and never read" — written by nothing, read by nothing, referenced
-- by nothing outside the migration that creates it, and empty in every database.
-- The shape was right and the feature was never built.
--
-- What it is for: the directory matches a homeowner's search against where a
-- contractor *is*, which is the address on their licence. That is a poor proxy
-- for where they work. A roofer in Culver City who covers the whole west side
-- is invisible to somebody searching Santa Monica, and a sole trader whose
-- licence carries their home address is placed at their house rather than at
-- their patch.
--
-- Two kinds of area, and the existing CHECK already says exactly one per row:
-- a named region ("I cover 90026 and 90042") or a radius from the contractor's
-- own point ("anywhere within 25 miles of me").

-- The radius of a circle with the same land area as the ZIP.
--
-- A stand-in for a boundary, and labelled as one. `regions.boundary` is a
-- MultiPolygon column that has been NULL since 0002, and filling it properly
-- means TIGER/Line shapefiles, a half-gigabyte download and a loader that does
-- not exist. This is derived from a number already in the Census gazetteer file
-- the centroids come from — sqrt(ALAND / pi) — so a region service area can be
-- matched today with one column and no new tooling.
--
-- What the approximation costs: a ZIP is not a circle, so this over-covers at
-- the corners and under-covers along a long thin one. For deciding whether a
-- contractor serves an area that is a reasonable trade; for anything that needs
-- to be exactly right, load the polygons and use `boundary`, which is why that
-- column is still there.
ALTER TABLE regions
    ADD COLUMN approx_radius_m integer CHECK (approx_radius_m > 0);

COMMENT ON COLUMN regions.approx_radius_m IS
    'Radius of the equal-area circle, standing in for boundary. See 0029.';

-- Matching reads (region_id -> centroid, radius) for every service area of
-- every candidate, so the lookup wants both columns without a heap fetch.
CREATE INDEX regions_zcta_radius_idx ON regions (id) INCLUDE (approx_radius_m)
    WHERE kind = 'zcta';

-- `contractor_service_areas` needs no change: 0010 got the shape right.
-- It gains its first index that leads with something other than the FK,
-- because matching asks "which contractors serve this point", not "what does
-- this contractor serve".
CREATE INDEX contractor_service_areas_radius_idx
    ON contractor_service_areas (radius_m)
    WHERE radius_m IS NOT NULL;
