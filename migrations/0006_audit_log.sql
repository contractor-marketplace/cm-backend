-- 0006 · Audit log.
--
-- Append-only. Rows outlive the actor: `actor_user_id` is nullable and set to
-- NULL when an account is deleted, because deleting an account must not erase
-- the record of what it did.
--
-- Generic over (subject_table, subject_id) so later milestones record their
-- events here without a schema change.

CREATE TABLE audit_log (
    id uuid PRIMARY KEY,
    actor_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    actor_kind text NOT NULL
        CHECK (actor_kind IN ('user', 'system', 'importer', 'admin')),
    -- Dotted event name, e.g. 'auth.login_succeeded'.
    action text NOT NULL CHECK (btrim(action) <> '' AND length(action) <= 100),
    subject_table text NOT NULL CHECK (btrim(subject_table) <> ''),
    subject_id uuid,
    -- Event detail. Never credentials, never a raw IP address, and never the
    -- email of an account that does not exist: a failed login against an
    -- unknown address records the reason and the peppered IP digest only, so
    -- the log does not accumulate the addresses of people who are not users.
    data jsonb NOT NULL DEFAULT '{}'::jsonb,
    request_id text CHECK (length(request_id) <= 64),
    ip_hash bytea CHECK (ip_hash IS NULL OR octet_length(ip_hash) = 32),
    -- No `updated_at`: rows are written once and never modified. The schema
    -- convention test names this table as the single documented exception.
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX audit_log_subject_idx ON audit_log (subject_table, subject_id, created_at DESC);
CREATE INDEX audit_log_created_at_idx ON audit_log (created_at DESC);
CREATE INDEX audit_log_actor_idx ON audit_log (actor_user_id, created_at DESC);
CREATE INDEX audit_log_action_idx ON audit_log (action, created_at DESC);
