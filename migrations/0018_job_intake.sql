-- 0018 · Jobs become a real intake form, and gain photos.
--
-- Before this, a job was a title, a description, and four optional fields. A
-- contractor reading the board could not tell a panel swap from a whole-house
-- rewire, because almost nothing was required and almost nothing was structured.
--
-- Every field is now required, and each one has a deliberate escape hatch so
-- that "I don't know" is an answer a person can give rather than a blank they
-- leave. The escapes are values, not absences, wherever there is a vocabulary to
-- put them in — `timeline = 'unsure'` and `build_type = 'unsure'` are recorded
-- choices, not missing data.
--
-- Two of them cannot be values, and for those ABSENCE IS THE ESCAPE:
--
--   * trade_id IS NULL          means "Other / not listed"
--   * both budget columns NULL  means "I'm not sure"
--
-- That reading is only sound because the API no longer accepts a request with a
-- field missing: a caller must send a trade or the literal "other", a budget or
-- the literal "unsure". Since a field can no longer be merely absent, NULL can
-- carry a meaning. `a_missing_field_is_refused` in the API tests is what keeps
-- that true, and it is the reason there is no companion "kind" column beside
-- either pair — such a column would record something already unambiguous.
--
-- The half-filled budget stops being representable at all, which it never should
-- have been: a job with a minimum and no maximum was neither a range nor an
-- absence of one.

BEGIN;

-- ── The new fields ────────────────────────────────────────────────────────
--
-- Both are added with a DEFAULT so the existing rows land somewhere legal, then
-- the DEFAULT is dropped. A default left in place would let a future code path
-- that forgets the column write a silent 'unsure' instead of failing, which is
-- exactly the class of bug the NOT NULL is here to catch.

ALTER TABLE jobs
    ADD COLUMN build_type text NOT NULL DEFAULT 'unsure'
        CHECK (build_type IN ('new_build', 'replacement', 'repair', 'unsure'));

ALTER TABLE jobs
    ADD COLUMN job_size text NOT NULL DEFAULT 'Not specified'
        CHECK (btrim(job_size) <> '' AND length(job_size) <= 200);

ALTER TABLE jobs ALTER COLUMN build_type DROP DEFAULT;
ALTER TABLE jobs ALTER COLUMN job_size DROP DEFAULT;

-- ── Timeline: a new vocabulary ────────────────────────────────────────────
--
-- "Within a month" and "within three months" both straddled the two-week line
-- this board actually cares about, and "flexible" was being used to mean two
-- different things — genuinely flexible, and not yet decided. The new set says
-- what a contractor needs to know: can I start now, this fortnight, or later,
-- and does this person even know yet.
--
-- Remapping is conservative. `within_a_month` becomes `more_than_2_weeks`
-- rather than `within_2_weeks`, because claiming more urgency than the poster
-- stated would put contractors in front of work that is not ready.

-- Order matters, and it is the reverse of the obvious one: the old CHECK has to
-- come off BEFORE the rows are remapped. Rewriting a row to 'unsure' while the
-- old vocabulary is still enforced fails on the very constraint this migration
-- is replacing.

ALTER TABLE jobs DROP CONSTRAINT jobs_timeline_check;

UPDATE jobs SET timeline = CASE timeline
    WHEN 'asap'                 THEN 'asap'
    WHEN 'within_a_month'       THEN 'more_than_2_weeks'
    WHEN 'within_three_months'  THEN 'more_than_2_weeks'
    WHEN 'flexible'             THEN 'unsure'
    ELSE 'unsure'
END;
UPDATE jobs SET timeline = 'unsure' WHERE timeline IS NULL;

ALTER TABLE jobs
    ADD CONSTRAINT jobs_timeline_check
        CHECK (timeline IN ('asap', 'within_2_weeks', 'more_than_2_weeks', 'unsure'));
ALTER TABLE jobs ALTER COLUMN timeline SET NOT NULL;

-- ── A ZIP is now required ─────────────────────────────────────────────────
--
-- There is no "I'm not sure" here, and there should not be: a person knows
-- their own postcode, and a job with no location is not findable by the only
-- search this board offers.
--
-- Requiring it does NOT mean requiring a *known* ZIP. A code outside the
-- imported ZCTA set still posts; it simply has no centroid, so it lands with
-- public_point_source = 'none' and appears in the list but not on the map. That
-- behaviour predates this migration and is unchanged.

UPDATE jobs SET postal_code = '00000' WHERE postal_code IS NULL;
ALTER TABLE jobs ALTER COLUMN postal_code SET NOT NULL;

-- ── The budget is a range or an admission ─────────────────────────────────

UPDATE jobs
   SET budget_min_cents = NULL, budget_max_cents = NULL
 WHERE (budget_min_cents IS NULL) <> (budget_max_cents IS NULL);

ALTER TABLE jobs
    ADD CONSTRAINT jobs_budget_is_a_range_or_nothing
        CHECK ((budget_min_cents IS NULL) = (budget_max_cents IS NULL));

-- ── A description worth reading ───────────────────────────────────────────
--
-- Fifty characters is roughly one sentence. It is not a quality bar; it is a
-- floor under "new panel" as an entire brief, which wastes the time of every
-- contractor who opens it.
--
-- Added as its own named constraint rather than by rewriting the original
-- inline one. That constraint is already applied under a generated name, and
-- editing an applied migration is refused by `an_edited_migration_is_rejected`.

ALTER TABLE jobs
    ADD CONSTRAINT jobs_description_is_substantial
        CHECK (length(btrim(description)) >= 50);

-- ── Photos ────────────────────────────────────────────────────────────────
--
-- The bytes live in object storage; this table is the index and the authority
-- on what is still visible. Only `storage_key` connects the two, and it is
-- built in one place (`cm_storage::photo_key`) so a delete can never disagree
-- with the put that created it.
--
-- Note what is NOT stored: any original file. Every upload is decoded and
-- re-encoded before it reaches storage, which discards EXIF — and therefore the
-- GPS coordinates a phone writes into a photograph of a house. This table has
-- no address column and neither does `jobs`; an unprocessed photo would have
-- handed back the address the schema was built to never hold. See the header of
-- crates/cm-storage/src/image.rs.
--
-- CASCADE, unlike the trade FK: a photo has no meaning without its job, and
-- deleting the job should take the row with it. The object itself is removed by
-- the application, which is the half a foreign key cannot do.

CREATE TABLE job_photos (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,

    -- Unique because two rows pointing at one object would make deleting either
    -- of them break the other.
    storage_key text NOT NULL UNIQUE
        CHECK (btrim(storage_key) <> '' AND length(storage_key) <= 500),

    -- One stored format, so there is one content type to serve and one thing to
    -- reason about. The normaliser re-encodes everything to JPEG.
    content_type text NOT NULL DEFAULT 'image/jpeg'
        CHECK (content_type = 'image/jpeg'),

    byte_size bigint NOT NULL CHECK (byte_size > 0),
    width integer NOT NULL CHECK (width > 0),
    height integer NOT NULL CHECK (height > 0),

    -- Display order, chosen by the poster's upload order. Zero-based.
    position integer NOT NULL CHECK (position >= 0),

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    -- Two photos cannot occupy one slot. This also indexes job_id as its
    -- leading column, which is what satisfies the foreign-key index invariant.
    CONSTRAINT job_photos_position_is_unique_per_job UNIQUE (job_id, position)
);

-- The per-job cap (eight) is enforced in the domain layer. A CHECK cannot count
-- rows in its own table, and a trigger that did would serialise concurrent
-- uploads to the same job for no benefit the application cannot get more
-- cheaply.

COMMIT;
