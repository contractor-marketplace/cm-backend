-- 0035 · An account does not need an email address to exist.
--
-- `users.email NOT NULL` was a v1 convenience that became a locked front
-- door. A Facebook account registered with a phone number, or one that
-- declined the email permission, arrives with no address anywhere — and the
-- only answer the schema allowed was to turn the person away.
--
-- The product decisions behind relaxing it, so the next reader does not
-- re-litigate them:
--
--   * Contact happens in the app. Messaging is the channel; email is a
--     notification convenience, and the mail that matters (job alerts) is
--     gated on a *verified* address anyway.
--   * Recovery is the provider subject, which keeps working, and support,
--     whose number is published. Losing a provider account is not a case the
--     schema needs to hold the door shut for.
--   * Identity never was the email. Accounts key on (provider, subject) —
--     0008's unique constraints — and the email's one cross-method job, the
--     collision tripwire, still fires whenever an address IS known. When one
--     genuinely is not, a duplicate account is accepted on purpose, and
--     link_provider is the sanctioned merge.
--
-- Everything else about the column already tolerates NULL: the format and
-- length CHECKs pass vacuously, the generated email_norm becomes NULL, and
-- the unique index ignores NULL rows — two email-less accounts coexist, which
-- is exactly the point. The migration test pins that.
ALTER TABLE users ALTER COLUMN email DROP NOT NULL;

COMMENT ON COLUMN users.email IS
    'Contact address, optional. NULL for a federated account whose provider '
    'shared no address; such accounts authenticate by provider subject and '
    'add an address later. Never used to find an account. See 0035.';

-- The verification code for an added or changed address leaves through the
-- outbox like all mail, under its own kind: reusing login_code would put
-- "your sign-in code" copy on an email that is not about signing in.
ALTER TABLE email_outbox DROP CONSTRAINT email_outbox_kind_check;
ALTER TABLE email_outbox ADD CONSTRAINT email_outbox_kind_check
    CHECK (kind IN ('login_code', 'password_reset', 'job_alert', 'email_verify'));
