-- 0036 · Delete the nightly verification log.
--
-- `verification_checks` was built to answer "who checked this, and when".
-- `recompute` then wrote a `cslb_license_active` row on every pass, including
-- every pass of `cm-verification.timer`, which runs nightly. The licence rows
-- it reads only move at import time, and imports are a hand-downloaded CSV
-- every few weeks — so each night it recorded ~50k rows (one per LA-county
-- listing) restating what the last import had already said, stamped with the
-- date the job ran rather than the date anybody looked at the register.
--
-- On a contractor's profile that rendered as twenty identical lines claiming
-- twenty consecutive days of checking. It was not checking. Nothing in this
-- system contacts CSLB on a schedule.
--
-- The code that wrote them is gone. This clears what they left behind.
--
-- Nothing is lost that was not already stored better:
--
--   * Why a badge is on or off — `contractors.verification_reason`, written on
--     the same pass, in English, naming the licence.
--   * When the register was last read — `licenses.last_seen_at`, and the
--     `license_import_runs` row behind it, which carries CSLB's own snapshot
--     date and the SHA-256 of the file.
--
-- Only the derived rows go. Rows recorded by a *person* — a claim decision, a
-- phone code, a mailed code — are the reason the table exists and are left
-- alone, which is why this is filtered on `kind` rather than a TRUNCATE.
DELETE FROM verification_checks WHERE kind = 'cslb_license_active';

-- `cslb_license_active` stays in the CHECK constraint on purpose. Removing it
-- would make a database migrated ahead of a not-yet-restarted binary reject
-- that binary's writes, and "a database ahead of the binary keeps serving" is
-- a property this deploy sequence relies on. The guard is that no code writes
-- the kind any more; the constraint is not the place to enforce it.
COMMENT ON TABLE verification_checks IS
    'Checks performed against a listing, one row per check. Rows are written '
    'when somebody or something actually verified something — a claim '
    'decision, a phone code, a mailed code. Never on a recompute: re-deriving '
    'a badge from stored licence facts observes nothing, and logging it once '
    'per contractor per night is what 0036 cleaned up. See cm-domain '
    'verification::recompute.';
