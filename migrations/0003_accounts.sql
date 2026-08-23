-- 0003 · Accounts and roles.
--
-- Identity is keyed by `id` (UUIDv7, generated in Rust). Email is a login
-- credential, never a join key: nothing in this schema or in any query may
-- match accounts by email, which is what stops an OAuth identity in a later
-- milestone from being merged into an existing account on an address alone.

CREATE TABLE users (
    id    uuid PRIMARY KEY,
    email text NOT NULL
        CHECK (length(email) <= 254)
        CHECK (email ~ '^[^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+$'),
    -- Normalisation is the database's job, not the application's: a generated
    -- column cannot drift from the value it is derived from, and the unique
    -- index below is what actually enforces one account per address.
    email_norm text GENERATED ALWAYS AS (lower(btrim(email))) STORED,
    email_verified_at timestamptz,
    display_name text NOT NULL
        CHECK (btrim(display_name) <> '' AND length(display_name) <= 120),
    -- TEXT + CHECK rather than a native enum: the values are asserted against
    -- the Rust enum by a migration test, which is the guarantee an enum was
    -- going to buy, without ALTER TYPE in a migration.
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'suspended', 'deleted')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX users_email_norm_key ON users (email_norm);

CREATE TABLE user_roles (
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role text NOT NULL
        CHECK (role IN ('homeowner', 'contractor', 'moderator', 'admin')),
    -- Nullable because roles granted by the operator CLI have no acting user.
    granted_by uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (user_id, role)
);

CREATE INDEX user_roles_granted_by_idx ON user_roles (granted_by);
