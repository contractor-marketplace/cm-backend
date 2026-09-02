-- 0027 · Let a contractor search the job board, and sort it.
--
-- The board has never had a search box. `JobFilters` carries a trade and a ZIP
-- and nothing else, so a contractor looking for bathroom work reads the list.
-- It has never had a sort either: `jobs_board_idx` backs one fixed ordering,
-- newest first, and that is the only order there has ever been.
--
-- The directory has had full text since 0010 and the two boards are the same
-- shape, so this is the same three pieces: a generated document to match
-- against, an index to match through, and one index per ordering whose columns
-- are exactly the ORDER BY tuple the cursor is built from.

-- Title carries more signal than description — somebody writes "Replace water
-- heater" and then three paragraphs of context — so it is weighted above it.
-- Generated and stored, like the contractor document, because the alternative
-- is recomputing a tsvector for every row of every search.
ALTER TABLE jobs
    ADD COLUMN search_doc tsvector
        GENERATED ALWAYS AS (
            setweight(to_tsvector('public.english_unaccent', coalesce(title, '')), 'A')
            || setweight(to_tsvector('public.english_unaccent', coalesce(description, '')), 'B')
        ) STORED;

CREATE INDEX jobs_search_doc_gin ON jobs USING GIN (search_doc);

-- One index per ordering, each holding the whole ORDER BY tuple and partial on
-- the only status the board shows. This is the pattern `jobs_board_idx` set in
-- 0017, and its comment states the rule these follow: the index tuple, the
-- ORDER BY tuple and the cursor tuple are the same three columns, or the scan
-- has to sort its own tail and the keyset stops being a keyset.
--
-- Budget sorts on the top of the range: a job posted at "up to $8,000" is worth
-- more to a contractor deciding what to read than one at "from $2,000", and a
-- job with no budget at all sorts last rather than as zero, which is what
-- NULLS LAST means here.
CREATE INDEX jobs_budget_idx
    ON jobs (budget_max_cents DESC NULLS LAST, created_at DESC, id DESC)
    WHERE status = 'open';

-- The facet counts read these directly. Small, low-cardinality columns, and the
-- counts run under the same predicate as the results on every search.
CREATE INDEX jobs_facet_idx
    ON jobs (trade_id, timeline, build_type)
    WHERE status = 'open';
