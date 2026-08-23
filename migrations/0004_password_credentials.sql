-- 0004 · Password credentials.
--
-- Separate from `users` so that an account without a password — a future
-- federated-only account — is a missing row rather than a nullable column that
-- every query has to reason about.

CREATE TABLE password_credentials (
    user_id uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    -- PHC string: algorithm, parameters and salt are all embedded, so raising
    -- the cost later is a rehash-on-next-login, not a schema change.
    password_hash text NOT NULL CHECK (password_hash LIKE '$argon2id$%'),
    failed_attempts integer NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    locked_until timestamptz,
    password_changed_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
