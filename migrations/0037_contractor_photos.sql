-- 0037 · Work photos on a claimed listing.
--
-- A claimant already has one profile photo, held in three columns on
-- `contractors`. This is the set of photographs of their work, and it is a copy
-- of `job_photos` (0018) for the reasons that table has the shape it has: the
-- row is the index and object storage holds the bytes; the normaliser
-- re-encodes to JPEG and discards EXIF, so a photograph taken inside a client's
-- house cannot carry that house's coordinates onto a public page; and the cap
-- lives in the domain layer because a CHECK cannot count rows in its own table.
--
-- CASCADE for the same reason as job photos: a photo has no meaning without its
-- listing. Listings are not deleted in practice, so this states the intent
-- rather than describing a path that runs.

BEGIN;

CREATE TABLE contractor_photos (
    id uuid PRIMARY KEY,
    contractor_id uuid NOT NULL REFERENCES contractors (id) ON DELETE CASCADE,

    -- Unique because two rows pointing at one object would make deleting either
    -- of them break the other.
    storage_key text NOT NULL UNIQUE
        CHECK (btrim(storage_key) <> '' AND length(storage_key) <= 500),

    -- One stored format, so there is one content type to serve.
    content_type text NOT NULL DEFAULT 'image/jpeg'
        CHECK (content_type = 'image/jpeg'),

    byte_size bigint NOT NULL CHECK (byte_size > 0),
    width integer NOT NULL CHECK (width > 0),
    height integer NOT NULL CHECK (height > 0),

    -- Display order, which is upload order. Zero-based.
    position integer NOT NULL CHECK (position >= 0),

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    -- Two photos cannot occupy one slot. Its leading column is also the index
    -- on contractor_id that the foreign-key index invariant asks for.
    CONSTRAINT contractor_photos_position_is_unique_per_contractor
        UNIQUE (contractor_id, position)
);

COMMIT;
