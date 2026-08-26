-- 0016 · Account type: an account is a homeowner or a contractor, never both.
--
-- This is deliberately NOT a role. Roles are granted, revoked and additive —
-- `contractor` is granted when a moderator approves a claim, and says the
-- holder proved they own a licensed business. Account type is chosen once at
-- registration, is mutually exclusive, and never changes. Modelling it as a
-- column rather than a row in user_roles is what makes "never both"
-- expressible: there is one value, so there cannot be two.
--
-- TEXT + CHECK rather than a native enum, matching users.status: the values
-- are asserted against the Rust enum by a migration test, which is the
-- guarantee an enum was going to buy, without ALTER TYPE in a migration.
--
-- The DEFAULT exists for two reasons and is kept on purpose. It backfills the
-- rows that predate this column, and it keeps this migration compatible with
-- the previously deployed binary, so `migrate` then `restart` is safe in
-- either order. The application always writes the value explicitly.
ALTER TABLE users
    ADD COLUMN account_type text NOT NULL DEFAULT 'homeowner'
        CHECK (account_type IN ('homeowner', 'contractor'));

-- Claim ownership is what actually grants a contractor their listing, and only
-- a contractor account may hold one. Enforcing it here as well as in the
-- application means a code path that forgets the check cannot create a
-- homeowner who owns a listing.
--
-- Written as a trigger rather than a CHECK because the condition spans two
-- tables: a CHECK constraint cannot read users from contractors.
CREATE OR REPLACE FUNCTION contractors_claimant_must_be_a_contractor()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.claimed_by_user_id IS NOT NULL THEN
        IF (SELECT account_type FROM users WHERE id = NEW.claimed_by_user_id) <> 'contractor' THEN
            RAISE EXCEPTION
                'a listing may only be claimed by a contractor account (user %)',
                NEW.claimed_by_user_id
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER contractors_claimant_is_a_contractor
    BEFORE INSERT OR UPDATE OF claimed_by_user_id ON contractors
    FOR EACH ROW
    EXECUTE FUNCTION contractors_claimant_must_be_a_contractor();

-- Homeowner profiles belong to homeowner accounts, for the same reason.
CREATE OR REPLACE FUNCTION homeowner_profiles_owner_must_be_a_homeowner()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF (SELECT account_type FROM users WHERE id = NEW.user_id) <> 'homeowner' THEN
        RAISE EXCEPTION
            'only a homeowner account may hold a homeowner profile (user %)',
            NEW.user_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER homeowner_profiles_owner_is_a_homeowner
    BEFORE INSERT OR UPDATE OF user_id ON homeowner_profiles
    FOR EACH ROW
    EXECUTE FUNCTION homeowner_profiles_owner_must_be_a_homeowner();
