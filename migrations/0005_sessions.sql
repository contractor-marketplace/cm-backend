-- 0005 · Sessions and single-use auth tokens.
--
-- Sessions are opaque random tokens stored only as their SHA-256 digest. A
-- database leak therefore yields no usable session: the digest cannot be
-- replayed, and 256 bits of entropy leaves nothing to brute-force. Deliberately
-- not JWTs — a session that cannot be revoked before its expiry is not a
-- session we can log out.

CREATE TABLE sessions (
    -- Not the token, and safe to log: the token never appears in this table.
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash bytea NOT NULL CHECK (octet_length(token_hash) = 32),
    -- Rolling: extended as the session is used, so an idle session dies.
    idle_expires_at timestamptz NOT NULL,
    -- Hard ceiling: a session in constant use still ends.
    absolute_expires_at timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz,
    revoked_reason text
        CHECK (revoked_reason IN
            ('logout', 'logout_all', 'password_change', 'rotation', 'admin')),
    -- Peppered digest, never the address itself.
    ip_hash bytea CHECK (ip_hash IS NULL OR octet_length(ip_hash) = 32),
    user_agent text CHECK (length(user_agent) <= 512),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT sessions_absolute_after_creation
        CHECK (absolute_expires_at > created_at),
    CONSTRAINT sessions_idle_within_absolute
        CHECK (idle_expires_at <= absolute_expires_at),
    CONSTRAINT sessions_revocation_is_complete
        CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL))
);

CREATE UNIQUE INDEX sessions_token_hash_key ON sessions (token_hash);
-- Full, not partial: this index also has to serve the cascade when a user row
-- is deleted, which must find revoked sessions too.
CREATE INDEX sessions_user_idx ON sessions (user_id);

-- Created now, used later. Password reset and email verification need a mail
-- path that does not exist yet, so no endpoint in v1 issues or consumes these
-- rows; the table lands with the rest of the auth schema so the flows are a
-- code change rather than a migration when the mail path is approved.
CREATE TABLE auth_tokens (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose text NOT NULL CHECK (purpose IN ('email_verify', 'password_reset')),
    token_hash bytea NOT NULL CHECK (octet_length(token_hash) = 32),
    expires_at timestamptz NOT NULL,
    consumed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX auth_tokens_token_hash_key ON auth_tokens (token_hash);
CREATE INDEX auth_tokens_user_idx ON auth_tokens (user_id);
