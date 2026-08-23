-- 0001 · Extensions and the text-search configuration.
--
-- Extension ownership: on the VPS these are created once by the migration/DDL
-- role, which is the only role with the privilege. `IF NOT EXISTS` makes this
-- migration a no-op when they are already present, so the runtime role never
-- needs the privilege.
--
-- citext is deliberately absent (plan review finding R1): email uniqueness uses
-- a generated `lower(btrim(email))` column so the normalisation is explicit at
-- every call site and visible in every query plan.

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS unaccent;

-- A text-search configuration with a constant name keeps `to_tsvector(regconfig,
-- text)` immutable, which is what allows it in a generated column and an index.
-- Calling unaccent() directly cannot be used there: it is not immutable.
--
-- CREATE TEXT SEARCH CONFIGURATION has no IF NOT EXISTS, so the duplicate is
-- swallowed to keep the file re-runnable by hand against an existing database.
DO $$
BEGIN
    CREATE TEXT SEARCH CONFIGURATION public.english_unaccent (COPY = pg_catalog.english);
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

ALTER TEXT SEARCH CONFIGURATION public.english_unaccent
    ALTER MAPPING FOR hword, hword_part, word
    WITH unaccent, english_stem;
