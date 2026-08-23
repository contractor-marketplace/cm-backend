-- 0009 · CSLB source data, kept separate from anything we author.
--
-- `license_records` mirrors the public register; `contractors` (next migration)
-- is our own record of a business. Keeping them apart is what lets an import
-- refresh source-derived fields without ever touching what a claimant wrote.

CREATE TABLE license_import_runs (
    id uuid PRIMARY KEY,
    source text NOT NULL CHECK (source IN ('cslb_master_list', 'cslb_county_list')),
    source_file_name text NOT NULL CHECK (btrim(source_file_name) <> ''),
    source_file_sha256 bytea NOT NULL CHECK (octet_length(source_file_sha256) = 32),
    -- CSLB's own "current as of" date, supplied by the operator.
    snapshot_date date,
    status text NOT NULL DEFAULT 'running'
        CHECK (status IN ('running', 'succeeded', 'failed')),
    rows_read integer NOT NULL DEFAULT 0 CHECK (rows_read >= 0),
    rows_inserted integer NOT NULL DEFAULT 0 CHECK (rows_inserted >= 0),
    rows_updated integer NOT NULL DEFAULT 0 CHECK (rows_updated >= 0),
    rows_unchanged integer NOT NULL DEFAULT 0 CHECK (rows_unchanged >= 0),
    rows_skipped integer NOT NULL DEFAULT 0 CHECK (rows_skipped >= 0),
    rows_rejected integer NOT NULL DEFAULT 0 CHECK (rows_rejected >= 0),
    error_text text,
    finished_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT license_import_runs_finished_iff_done
        CHECK ((status = 'running') = (finished_at IS NULL))
);

-- File-level idempotency: the same bytes cannot succeed twice.
CREATE UNIQUE INDEX license_import_runs_file_once
    ON license_import_runs (source, source_file_sha256) WHERE status = 'succeeded';
CREATE INDEX license_import_runs_recent_idx ON license_import_runs (created_at DESC);

CREATE TABLE license_records (
    id uuid PRIMARY KEY,
    -- CSLB's own key.
    license_no text NOT NULL CHECK (btrim(license_no) <> '' AND length(license_no) <= 32),
    business_name text NOT NULL CHECK (btrim(business_name) <> ''),
    business_type text CHECK (length(business_type) <= 120),
    status text NOT NULL
        CHECK (status IN ('active', 'expired', 'suspended', 'inactive', 'unknown')),
    -- CSLB's string, unmapped, so a mapping mistake is repairable without
    -- re-downloading the file.
    status_raw text NOT NULL,
    issue_date date,
    expiration_date date,
    classifications text[] NOT NULL DEFAULT '{}',
    bond_amount_cents bigint CHECK (bond_amount_cents >= 0),
    workers_comp_status text CHECK (length(workers_comp_status) <= 120),
    address_line1 text,
    city text,
    state text CHECK (length(state) <= 2),
    postal_code text CHECK (length(postal_code) <= 10),
    county text,
    phone text CHECK (length(phone) <= 32),
    -- The source row, verbatim.
    raw jsonb NOT NULL,
    -- Digest of the normalised row; drives change detection.
    content_hash bytea NOT NULL CHECK (octet_length(content_hash) = 32),
    first_run_id uuid NOT NULL REFERENCES license_import_runs (id) ON DELETE RESTRICT,
    last_run_id uuid NOT NULL REFERENCES license_import_runs (id) ON DELETE RESTRICT,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT license_records_license_no_key UNIQUE (license_no)
);

CREATE INDEX license_records_county_idx ON license_records (county);
CREATE INDEX license_records_status_idx ON license_records (status);
CREATE INDEX license_records_first_run_idx ON license_records (first_run_id);
CREATE INDEX license_records_last_run_idx ON license_records (last_run_id);

-- Append-only history. "Preserve the raw source" has to survive import #2.
CREATE TABLE license_record_versions (
    id uuid PRIMARY KEY,
    license_record_id uuid NOT NULL REFERENCES license_records (id) ON DELETE CASCADE,
    run_id uuid NOT NULL REFERENCES license_import_runs (id) ON DELETE RESTRICT,
    content_hash bytea NOT NULL CHECK (octet_length(content_hash) = 32),
    raw jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    -- An unchanged row adds nothing.
    CONSTRAINT license_record_versions_unique_content
        UNIQUE (license_record_id, content_hash)
);

CREATE INDEX license_record_versions_run_idx ON license_record_versions (run_id);
