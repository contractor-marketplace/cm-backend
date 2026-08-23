-- 0014 · Direct messaging.
--
-- Two decisions carry the weight here.
--
-- Participants are a table rather than two columns, so a stage-2 job-scoped
-- thread with more than two people needs no rewrite. But a two-party thread
-- also needs "one conversation per pair" to be race-proof, which a participant
-- table cannot express — hence the canonically ordered pair columns and the
-- unique index over them.
--
-- Ordering uses a per-conversation sequence, not a timestamp. Polling with
-- `created_at > $cursor` loses messages: a transaction that started earlier can
-- commit later, so a row can appear behind a cursor the client has passed.

CREATE TABLE conversations (
    id uuid PRIMARY KEY,
    kind text NOT NULL DEFAULT 'dm' CHECK (kind IN ('dm')),
    -- Which contractor the conversation is about. Context, not identity.
    contractor_id uuid REFERENCES contractors (id) ON DELETE RESTRICT,
    dm_lo uuid REFERENCES users (id) ON DELETE RESTRICT,
    dm_hi uuid REFERENCES users (id) ON DELETE RESTRICT,
    -- Gapless, assigned under this row's lock.
    last_seq bigint NOT NULL DEFAULT 0 CHECK (last_seq >= 0),
    last_message_at timestamptz,
    created_by uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT conversations_dm_pair_is_ordered
        CHECK (kind <> 'dm' OR (dm_lo IS NOT NULL AND dm_hi IS NOT NULL AND dm_lo < dm_hi))
);

CREATE UNIQUE INDEX conversations_dm_pair_key
    ON conversations (dm_lo, dm_hi) WHERE kind = 'dm';
CREATE INDEX conversations_contractor_idx ON conversations (contractor_id);
CREATE INDEX conversations_created_by_idx ON conversations (created_by);
CREATE INDEX conversations_dm_hi_idx ON conversations (dm_hi);
CREATE INDEX conversations_recent_idx ON conversations (last_message_at DESC NULLS LAST);

CREATE TABLE conversation_participants (
    conversation_id uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role text NOT NULL CHECK (role IN ('initiator', 'recipient')),
    last_read_seq bigint NOT NULL DEFAULT 0 CHECK (last_read_seq >= 0),
    muted boolean NOT NULL DEFAULT false,
    left_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (conversation_id, user_id)
);

CREATE INDEX conversation_participants_user_idx ON conversation_participants (user_id);

CREATE TABLE messages (
    id uuid PRIMARY KEY,
    conversation_id uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    seq bigint NOT NULL CHECK (seq > 0),
    sender_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    body text NOT NULL CHECK (btrim(body) <> '' AND length(body) <= 4000),
    edited_at timestamptz,
    deleted_at timestamptz,
    deleted_by uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT messages_seq_key UNIQUE (conversation_id, seq)
);

CREATE INDEX messages_sender_idx ON messages (sender_user_id);
CREATE INDEX messages_deleted_by_idx ON messages (deleted_by);
