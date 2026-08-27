-- 0024 · What a contractor owns about their own listing.
--
-- Until now a claimant could edit four things: bio, website, public phone and
-- whether they take messages. Everything else on the page was the CSLB
-- register's. This adds the rest of a real profile: their own address, links to
-- the review pages they already have elsewhere, and a photo.
--
--
-- THE ADDRESS IS AN OVERRIDE, AND THAT IS A DELIBERATE REVERSAL
--
-- `contractors.rs` and the profile page both say the address is the licence's
-- and that a correction goes through the CSLB. That was the right default for a
-- directory nobody had claimed. It is the wrong one for a claimed listing: the
-- register lags reality by months, sole traders are listed at addresses they
-- have moved out of, and a contractor who cannot fix their own address on their
-- own profile will conclude the product is broken.
--
-- So an approved claimant may set an address, and when they do it REPLACES the
-- licence address in every read path. The trade is real and worth naming: a
-- listing can now display an address the register contradicts. Two things keep
-- that honest rather than merely permissive.
--
--   1. Only an approved claimant can write these columns. The handler checks
--      it, and an unclaimed listing has no way to acquire an owner address.
--   2. The licence address is not overwritten, deleted, or edited. It stays on
--      `license_records` exactly as imported, so verification still anchors to
--      the register and the next import still lands cleanly.
--
-- The pin follows the displayed address. Leaving the map on the licence address
-- while the page showed another would reintroduce, in a new place, exactly the
-- bug the location invariant exists to prevent: search and map disagreeing
-- about where somebody is. `geocodable_address` therefore prefers these columns
-- and a change re-enqueues geocoding.

ALTER TABLE contractors
    ADD COLUMN owner_address_line1 text,
    ADD COLUMN owner_address_city text,
    ADD COLUMN owner_address_state text,
    ADD COLUMN owner_address_postal_code text,

    -- Where they already have reviews. Stored as links, not scraped: the
    -- contractor is asserting "this is my page", which is a different and much
    -- safer claim than our matcher guessing it — and the 0.55-floor experiment
    -- in tools/gmaps-enrichment/publish.sql is a standing reminder of how badly
    -- the guessing goes.
    --
    -- `google_review_url` is distinct from `google_place_url` from 0023. That
    -- one is ours, written by the promotion step from a match we made. This one
    -- is theirs. When both exist the contractor's wins, because they know and
    -- we inferred.
    ADD COLUMN google_review_url text,
    ADD COLUMN yelp_url text,

    -- One profile photo, not a gallery. Columns rather than a table because
    -- there is exactly one, and a `contractor_photos` table with a partial
    -- unique index enforcing "at most one" would be a table pretending to be a
    -- column.
    --
    -- The object is EXIF-stripped by re-encode on the way in, the same pass job
    -- photos go through — see crates/cm-storage/src/image.rs. That matters here
    -- for the same reason it mattered there: a photo taken at the business
    -- carries the coordinates of the business.
    ADD COLUMN photo_storage_key text,
    ADD COLUMN photo_width integer CHECK (photo_width > 0),
    ADD COLUMN photo_height integer CHECK (photo_height > 0);

-- An address is all four parts or none of them, so a half-filled state cannot
-- be reached. Without this, a contractor who cleared only the city would get a
-- geocodable string that silently resolves to the wrong place.
ALTER TABLE contractors ADD CONSTRAINT contractors_owner_address_is_whole
    CHECK (
        (owner_address_line1 IS NULL AND owner_address_city IS NULL
         AND owner_address_state IS NULL AND owner_address_postal_code IS NULL)
        OR
        (owner_address_line1 IS NOT NULL AND owner_address_city IS NOT NULL
         AND owner_address_state IS NOT NULL AND owner_address_postal_code IS NOT NULL)
    );

-- The photo is a key plus its dimensions, or nothing. A key with no dimensions
-- renders as a layout shift; dimensions with no key render as nothing at all.
ALTER TABLE contractors ADD CONSTRAINT contractors_photo_is_whole
    CHECK (
        (photo_storage_key IS NULL AND photo_width IS NULL AND photo_height IS NULL)
        OR
        (photo_storage_key IS NOT NULL AND photo_width IS NOT NULL AND photo_height IS NOT NULL)
    );

-- One object per listing. Catches a double-upload that wrote a second key
-- without deleting the first, which would leak the old object forever.
CREATE UNIQUE INDEX contractors_photo_storage_key_idx
    ON contractors (photo_storage_key) WHERE photo_storage_key IS NOT NULL;
