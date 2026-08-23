-- 0011 · The geocoding queue.
--
-- The importer only enqueues; it never calls a network. Workers claim rows with
-- FOR UPDATE SKIP LOCKED, so running two of them is safe and neither blocks the
-- other.

CREATE TABLE geocode_queue (
    id uuid PRIMARY KEY,
    contractor_id uuid NOT NULL REFERENCES contractors (id) ON DELETE CASCADE,
    -- Digest of the normalised address being resolved, so a re-import that did
    -- not change the address does not re-queue the work.
    address_hash bytea NOT NULL CHECK (octet_length(address_hash) = 32),
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'in_progress', 'succeeded', 'failed', 'skipped')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    locked_at timestamptz,
    locked_by text CHECK (length(locked_by) <= 120),
    provider text CHECK (length(provider) <= 60),
    provider_response jsonb,
    last_error text CHECK (length(last_error) <= 2000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- At most one open job per contractor: re-queuing an address already waiting
-- is a no-op rather than a second row.
CREATE UNIQUE INDEX geocode_queue_one_open
    ON geocode_queue (contractor_id) WHERE status IN ('queued', 'in_progress');
CREATE INDEX geocode_queue_ready_idx
    ON geocode_queue (next_attempt_at) WHERE status = 'queued';
CREATE INDEX geocode_queue_contractor_idx ON geocode_queue (contractor_id);
