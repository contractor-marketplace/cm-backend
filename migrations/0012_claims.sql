-- 0012 · Claims and the evidence behind a verified badge.
--
-- Evidence is typed rows, not free text, because "why is this contractor
-- verified" has to be answerable months later by someone who was not there.

CREATE TABLE contractor_claims (
    id uuid PRIMARY KEY,
    contractor_id uuid NOT NULL REFERENCES contractors (id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'rejected', 'withdrawn')),
    method text NOT NULL
        CHECK (method IN ('license_phone_otp', 'license_mail_code', 'manual_review')),
    -- What the claimant asserted. Never trusted on its own.
    evidence jsonb NOT NULL DEFAULT '{}'::jsonb,
    decided_at timestamptz,
    decided_by uuid REFERENCES users (id) ON DELETE SET NULL,
    decision_note text CHECK (length(decision_note) <= 2000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT contractor_claims_decided_iff_not_pending
        CHECK ((status = 'pending') = (decided_at IS NULL))
);

-- The three invariants that make concurrent approvals safe, enforced by the
-- database rather than by whoever writes the handler.
CREATE UNIQUE INDEX contractor_claims_one_pending_per_pair
    ON contractor_claims (contractor_id, user_id) WHERE status = 'pending';
CREATE UNIQUE INDEX contractor_claims_one_approved_per_contractor
    ON contractor_claims (contractor_id) WHERE status = 'approved';
CREATE UNIQUE INDEX contractor_claims_one_approved_per_user
    ON contractor_claims (user_id) WHERE status = 'approved';
CREATE INDEX contractor_claims_user_idx ON contractor_claims (user_id);
CREATE INDEX contractor_claims_decided_by_idx ON contractor_claims (decided_by);
CREATE INDEX contractor_claims_queue_idx ON contractor_claims (status, created_at);

CREATE TABLE verification_checks (
    id uuid PRIMARY KEY,
    contractor_id uuid NOT NULL REFERENCES contractors (id) ON DELETE CASCADE,
    claim_id uuid REFERENCES contractor_claims (id) ON DELETE SET NULL,
    kind text NOT NULL CHECK (kind IN (
        'cslb_license_active',
        'cslb_bond_present',
        'cslb_workers_comp',
        'phone_otp',
        'mail_code',
        'manual_review'
    )),
    outcome text NOT NULL CHECK (outcome IN ('pass', 'fail', 'inconclusive')),
    evidence jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Which import this observation came from, when it came from one.
    source_run_id uuid REFERENCES license_import_runs (id) ON DELETE SET NULL,
    performed_by uuid REFERENCES users (id) ON DELETE SET NULL,
    observed_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX verification_checks_recent_idx
    ON verification_checks (contractor_id, observed_at DESC);
CREATE INDEX verification_checks_claim_idx ON verification_checks (claim_id);
CREATE INDEX verification_checks_run_idx ON verification_checks (source_run_id);
CREATE INDEX verification_checks_performer_idx ON verification_checks (performed_by);
