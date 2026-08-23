-- 0015 · Blocking and reporting.

CREATE TABLE user_blocks (
    blocker_user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    blocked_user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    reason text CHECK (length(reason) <= 500),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (blocker_user_id, blocked_user_id),
    CONSTRAINT user_blocks_not_self CHECK (blocker_user_id <> blocked_user_id)
);

-- The reverse direction is queried on every send: a block stops messages both
-- ways, so both columns need to be searchable.
CREATE INDEX user_blocks_blocked_idx ON user_blocks (blocked_user_id);

CREATE TABLE message_reports (
    id uuid PRIMARY KEY,
    -- RESTRICT, not SET NULL: an open moderation case must not be erasable by
    -- deleting the reporting account.
    reporter_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    conversation_id uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    message_id uuid REFERENCES messages (id) ON DELETE SET NULL,
    reason text NOT NULL CHECK (reason IN
        ('spam', 'harassment', 'scam', 'off_platform_payment', 'other')),
    detail text CHECK (length(detail) <= 2000),
    status text NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'reviewing', 'actioned', 'dismissed')),
    reviewed_at timestamptz,
    reviewed_by uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX message_reports_one_per_reporter_message
    ON message_reports (reporter_user_id, message_id) WHERE message_id IS NOT NULL;
CREATE INDEX message_reports_queue_idx ON message_reports (status, created_at);
CREATE INDEX message_reports_conversation_idx ON message_reports (conversation_id);
CREATE INDEX message_reports_message_idx ON message_reports (message_id);
CREATE INDEX message_reports_reporter_idx ON message_reports (reporter_user_id);
CREATE INDEX message_reports_reviewer_idx ON message_reports (reviewed_by);
