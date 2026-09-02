-- 0033 · Login codes join the auth-token vocabulary.
--
-- auth_tokens has waited unused since 0005 for exactly this: the mail path now
-- exists (0032), so the table gains its first writer. The auth model decided
-- with the product owner: a 6-digit emailed code at sign-up and at log-in from
-- an unrecognized browser. The code round-trip is the email verification —
-- there is no separate verify-link flow, so the 'email_verify' purpose stays
-- in the vocabulary unwritten rather than being churned out of a CHECK an
-- older binary still knows.
--
-- attempts is new: a 6-digit code has a millionth of the entropy of a reset
-- link, so it dies after a handful of wrong guesses rather than living out its
-- expiry window as a brute-force target.

ALTER TABLE auth_tokens DROP CONSTRAINT auth_tokens_purpose_check;
ALTER TABLE auth_tokens ADD CONSTRAINT auth_tokens_purpose_check
    CHECK (purpose IN ('email_verify', 'login_code', 'password_reset'));

ALTER TABLE auth_tokens ADD COLUMN attempts integer NOT NULL DEFAULT 0
    CHECK (attempts >= 0);

-- What the prune sweep scans: open tokens past their expiry.
CREATE INDEX auth_tokens_expiry_idx ON auth_tokens (expires_at)
    WHERE consumed_at IS NULL;
