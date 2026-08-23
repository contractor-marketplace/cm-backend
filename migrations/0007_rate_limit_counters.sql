-- 0007 · Rate-limit counters.
--
-- Durable rather than in-process: an in-memory limiter resets on every deploy,
-- which turns a restart into a free window for whoever is being limited.
--
-- The bucket key is stored as a peppered digest, never in the clear. A bucket
-- names an IP address or a user id, and neither belongs in a table that exists
-- only to count requests.

CREATE TABLE rate_limit_counters (
    bucket_hash bytea NOT NULL CHECK (octet_length(bucket_hash) = 32),
    -- Fixed window. The pair is the primary key, so the counter update is a
    -- single atomic upsert with no read-modify-write to lose.
    window_start timestamptz NOT NULL,
    count integer NOT NULL DEFAULT 0 CHECK (count >= 0),
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (bucket_hash, window_start),
    CONSTRAINT rate_limit_counters_expiry_after_window
        CHECK (expires_at > window_start)
);

-- Drives the bounded sweep that deletes elapsed windows.
CREATE INDEX rate_limit_counters_expires_at_idx ON rate_limit_counters (expires_at);
