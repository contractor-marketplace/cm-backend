-- 0008 · Federated identities.
--
-- Keyed by (provider, subject) and nothing else. There is deliberately no index
-- on, constraint over, or query against the email column: matching accounts by
-- address is the account-takeover vector this whole table exists to avoid.
--
-- For Google the subject stored is the *Google* account id, taken from the
-- verified token's `firebase.identities`, not the Firebase uid. That value is
-- what a direct Google OIDC integration would also return, so dropping Firebase
-- later re-links nobody.

CREATE TABLE oauth_identities (
    id      uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    provider text NOT NULL CHECK (provider IN ('google')),
    subject  text NOT NULL CHECK (btrim(subject) <> '' AND length(subject) <= 255),
    -- Support and debugging only. Never a join key, never matched on.
    firebase_uid text CHECK (length(firebase_uid) <= 128),
    -- What the provider asserted at link time, kept for audit. Not a lookup key.
    email_at_link text CHECK (length(email_at_link) <= 254),
    email_verified_at_link boolean NOT NULL DEFAULT false,
    last_login_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT oauth_identities_provider_subject_key UNIQUE (provider, subject),
    CONSTRAINT oauth_identities_one_per_provider UNIQUE (user_id, provider)
);

CREATE INDEX oauth_identities_user_idx ON oauth_identities (user_id);
