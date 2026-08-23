-- 0013 · Homeowner profiles.
--
-- Optional: an account with no profile row is a valid mid-onboarding state, not
-- an error. Role is implied by which profile exists, not by a column on `users`.

CREATE TABLE homeowner_profiles (
    user_id uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND length(display_name) <= 120),
    postal_code text CHECK (postal_code ~ '^[0-9]{5}$'),
    region_id uuid REFERENCES regions (id) ON DELETE SET NULL,
    contact_phone text CHECK (length(contact_phone) <= 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX homeowner_profiles_region_idx ON homeowner_profiles (region_id);
