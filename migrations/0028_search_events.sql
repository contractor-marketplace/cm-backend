-- 0028 · What people actually did with the results, and which job a
--        conversation is about.
--
-- Every ranking change so far has been judged against a golden set — queries
-- with hand-labelled right answers. That is the correct instrument for "does
-- this find the right things", and it is the wrong one for "do people click
-- them". A golden set is one person's opinion written down once; this table is
-- everybody's behaviour, recorded continuously.
--
-- It exists before the ranking that needs it, deliberately. Personalised
-- ranking cannot be tuned without interaction data and learned ranking cannot
-- be trained on it at all, so the order is: log, wait, then rank. Shipping the
-- ranking first would mean months of guessing followed by starting the clock.

CREATE TABLE search_events (
    id           uuid PRIMARY KEY,

    -- What happened. An impression is "this was on a page somebody saw", a
    -- click is "they opened it", a contact is "they acted on it". The three
    -- together give a rate per position, which is the whole point: a result
    -- that is always shown and never opened is ranked too high.
    kind         text NOT NULL CHECK (kind IN ('impression', 'click', 'contact')),

    -- Which board. The directory and the job board rank differently and have
    -- to be measured apart.
    surface      text NOT NULL CHECK (surface IN ('directory', 'jobs')),

    -- The contractor or job the event is about. Not a foreign key, because it
    -- points at two different tables and because an event about something
    -- since deleted is still evidence about the ranking that showed it.
    subject_id   uuid NOT NULL,

    -- Who, when there is a who. Browsing the directory needs no session, so
    -- most impressions have none, and that is not a gap to be filled.
    actor_user_id uuid REFERENCES users (id) ON DELETE SET NULL,

    -- Where it sat on the page, one-based. The number that makes a click rate
    -- comparable between queries: position two of three is not position two of
    -- four hundred.
    position     integer NOT NULL CHECK (position >= 1),

    -- The shape of the search, never its text. `router.rs` keeps query strings
    -- out of its spans because they carry what somebody typed, often their own
    -- name or address, and a table that is kept for months is a worse place for
    -- that than a log line that rotates. Enough to answer "did ranking get
    -- better for searches with a query" without keeping the queries.
    had_query    boolean NOT NULL DEFAULT false,
    sort         text,

    -- Ties the three kinds together: an impression and the click that followed
    -- it share one, so a click can be attributed to the page that produced it
    -- without joining on time.
    request_id   text,

    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Append-only, like `audit_log`. There is no `updated_at` for the same reason:
-- an event does not change, so the column could only ever lie. The invariant
-- test carries a matching exemption.

-- The query every analysis starts with: this surface, this window.
CREATE INDEX search_events_surface_idx ON search_events (surface, created_at DESC);
-- Rate per position, per kind.
CREATE INDEX search_events_kind_idx ON search_events (kind, position);
-- Leads with the foreign key, per the schema invariant.
CREATE INDEX search_events_actor_idx ON search_events (actor_user_id);
-- Attribution: the events of one page, together.
CREATE INDEX search_events_request_idx ON search_events (request_id)
    WHERE request_id IS NOT NULL;

-- Which job a conversation is about.
--
-- `messaging::start_with_job` already resolves a job to its poster and opens a
-- conversation with them, and then forgets which job it was. That makes an
-- obvious question unanswerable — how many contractors have already replied to
-- this posting — and it is the signal a lead feed most needs, because a job
-- with nine replies is worth less to the tenth contractor than one with none.
ALTER TABLE conversations
    ADD COLUMN job_id uuid REFERENCES jobs (id) ON DELETE SET NULL;

CREATE INDEX conversations_job_idx ON conversations (job_id) WHERE job_id IS NOT NULL;
