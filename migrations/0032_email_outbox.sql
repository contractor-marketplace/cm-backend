-- 0032 · The email outbox.
--
-- The first mail path in the product. Mail is enqueued in the same transaction
-- that creates the reason for it — a sign-in code, a reset link, a job digest —
-- and delivered by a worker. That ordering is the whole design: a crashed
-- request or a provider outage delays mail, it never loses it, and no request
-- handler ever waits on a mail provider.
--
-- Bodies are rendered at enqueue time, not send time. The values a body
-- carries (a code, a single-use link) exist only in the transaction that
-- issues them, and a worker that merely posts finished bodies cannot render
-- them wrongly later against changed state.
--
-- The shape is the geocode queue's (0011): claim with FOR UPDATE SKIP LOCKED,
-- attempts with backoff, a stalled-claim recovery path. Rows in terminal
-- states are pruned on the standard grace — a sent body still contains the
-- reset link it carried, so keeping it forever would be keeping credentials.

CREATE TABLE email_outbox (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- The address at enqueue time. Copied rather than joined at send time so a
    -- user who changes their address mid-flight gets the mail where the flow
    -- that enqueued it addressed it.
    recipient text NOT NULL CHECK (length(recipient) <= 320),
    kind text NOT NULL CHECK (kind IN ('login_code', 'password_reset', 'job_alert')),
    subject text NOT NULL CHECK (length(subject) <= 300),
    body_text text NOT NULL CHECK (length(body_text) <= 100000),
    body_html text CHECK (length(body_html) <= 200000),
    -- job_alert only: becomes the List-Unsubscribe headers on the message.
    unsubscribe_url text CHECK (length(unsubscribe_url) <= 2000),
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'in_progress', 'sent', 'failed')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    locked_at timestamptz,
    locked_by text CHECK (length(locked_by) <= 120),
    provider_message_id text CHECK (length(provider_message_id) <= 200),
    last_error text CHECK (length(last_error) <= 2000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- What the worker polls: due, queued rows. Partial, so the index carries only
-- the working set rather than every message ever sent.
CREATE INDEX email_outbox_ready_idx ON email_outbox (next_attempt_at)
    WHERE status = 'queued';

CREATE INDEX email_outbox_user_idx ON email_outbox (user_id);
