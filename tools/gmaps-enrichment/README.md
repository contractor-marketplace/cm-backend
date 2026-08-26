# Google Maps enrichment

Resolves each CSLB contractor to a Google Maps place, verifies the match, pulls
up to 50 reviews, and writes them to staging Postgres.

Staging load only. Nothing here writes to the chain or to SNIP, and nothing here
generates synthetic data — that is a separate job behind its own flag.

## Running it

```bash
cd tools/gmaps-enrichment
python3 -m venv .venv && .venv/bin/pip install psycopg2-binary

export APIFY_TOKEN=...                     # never logged, never stored
export APIFY_ACTOR_ID=gT99sk2Z5BOn6jD7M
export DATABASE_URL=postgres://cm_migrate:...@127.0.0.1/cm

.venv/bin/python ingest.py --limit 100     # start small
```

`DATABASE_URL` must be a role that can create tables — `cm_migrate`, not the
service role `cm_app`, which deliberately cannot.

| Flag | Default | |
|---|---|---|
| `--limit N` | whole list | stop after N contractors |
| `--max-spend USD` | 50 | halt when Apify's reported spend passes this |
| `--retry-after-days N` | 30 | re-attempt a previously tried contractor after N days |
| `--dry-run` | | everything except calling Apify |
| `--schema-only` | | create the staging schema and exit |

Safe to interrupt. SIGINT and SIGTERM finish the batch in flight and stop, so no
run is left recorded as `running` with a dataset nobody fetches.

## The $50 cap will bind long before the list is exhausted

49,774 contractors is 2,489 actor runs, each spinning up three proxied browsers.
$50 realistically covers somewhere between 600 and 2,000 of them. That is not a
bug in the job and not a reason to raise the cap — it is what the work costs.

The job is built for it. A contractor is selected only when it has no confirmed
match and has not been attempted within `--retry-after-days`, so each run picks
up where the last one stopped. Run it repeatedly, raising `--max-spend` when you
decide to spend more.

Spend is Apify's own `usageTotalUsd` per run, accumulated in
`staging.scrape_runs.cost_usd` — not a locally modelled estimate, which would be
wrong the first time the actor's pricing changed.

## Where the data lands

Everything is in a **`staging` schema**, not `public`.

`public` is the product schema, and cm-db's migration tests enforce four
invariants on it: created_at/updated_at on every table, every foreign key
single-column and indexed, no database default on a UUID primary key, four
extensions. Those tests scope themselves to `public`. The tables in this spec do
not satisfy them — `gmaps_places` has first/last_scraped_at rather than
created/updated_at — so creating them in `public` would fail the product suite,
and reshaping them would mean not building what was asked for. A separate schema
resolves that honestly, and says the true thing: these are ETL artefacts with
their own lifecycle. Cross-schema foreign keys to `public.contractors(id)` work
exactly as within-schema ones do.

| Table | |
|---|---|
| `staging.gmaps_places` | one row per Google place, upserted |
| `staging.gmaps_reviews` | one row per review, insert-only |
| `staging.contractor_place_matches` | confirmed / needs_review / rejected, with score components |
| `staging.place_match_attempts` | every attempt including no-results, for audit and for not re-querying |
| `staging.scrape_runs` | one row per Apify run, with its dataset id and cost |

The one product-schema change, `contractors.data_source`, is a normal cm-backend
migration (`0020`) rather than an ALTER from this tool. A column added outside
the migration system would exist on this deployment and on no fresh one.

**Reviews are immutable.** `ON CONFLICT DO NOTHING`, never `DO UPDATE`. If a
review is edited on Google after we captured it, our original is what we keep —
an audit trail that silently rewrites itself is not one.

## Match verification

`locationNames` is a fuzzy search. Google returns *something* for almost any
query, and that something is routinely the wrong business. An unverified match
is worse than no match, because it welds a stranger's reviews onto a real
licensed contractor and nothing downstream can tell.

Weights, from the spec: name 0.4, city 0.3, state 0.2, category 0.1.
`>= 0.75` confirmed, `0.50–0.75` needs_review, below that rejected. A state that
is not CA rejects regardless of total.

**One addition, and it tightens rather than loosens.** City (0.3) plus state
(0.2) is exactly 0.50 — the needs_review floor. So under the weights as given,
*any* place in the right city and state clears the bar with a name score of zero
and an implausible category, and needs_review writes reviews. A nail salon two
doors down from the plumber would have had its reviews attached to that plumber.
`matching.NAME_FLOOR` rejects below 0.35 name similarity, on the same
"forces a reject" pattern the spec already establishes for state. Measured
against real pairs the separation is clean: unrelated businesses score
0.105–0.263, the weakest plausible match ("ABC Plumbing" vs "ABC Electric",
possibly the same owner) scores 0.417, genuine matches 0.727–1.000.

**Known weakness.** Short generic names defeat this. "GO ELECTRIC" against "Gold
Coast Electric" scores 0.727, which with a matching city and a plausible
category confirms at 0.89. Nothing in the four signals can separate those. If
the confirmed set matters more than the match rate, review the confirmed rows
whose `score_components.name_similarity` is between 0.6 and 0.8.

Expect 30–60% of contractors to have no usable Google presence. That is the
normal outcome. Do not lower the thresholds to improve the number — every
component of every score is stored, so a low rate can be diagnosed instead.

```sql
-- which signal is failing
SELECT outcome,
       count(*),
       round(avg((score_components->>'name_similarity')::numeric), 3) AS avg_name,
       round(avg((score_components->>'city_match')::numeric), 3)      AS avg_city
  FROM staging.place_match_attempts
 GROUP BY outcome ORDER BY 2 DESC;

-- confirmed matches worth a human glance
SELECT c.display_name, p.place_name, m.match_score
  FROM staging.contractor_place_matches m
  JOIN public.contractors c ON c.id = m.contractor_id
  JOIN staging.gmaps_places p ON p.place_id = m.place_id
 WHERE m.match_status = 'confirmed'
   AND (m.score_components->>'name_similarity')::numeric < 0.8
 ORDER BY m.match_score;
```

## Tests

```bash
python3 -m unittest test_matching test_apify          # no database, no network
DATABASE_URL=... .venv/bin/python -m unittest test_pipeline   # real DB, rolls back
```

`test_pipeline` runs against a real database and rolls back every test. That is
deliberate: foreign keys, conflict targets, JSONB casts and NOT NULL constraints
are exactly what a fixture-only test cannot check, and exactly what fails on
batch 200.

## Notes on the actor's output

- One row per review, place fields repeated on every row. `ingest.split_dataset`
  groups by `placeId`.
- `publishedAt` is relative text — "2 months ago" — and is never parsed. Only
  `publishedAtDate` reaches a timestamp column; when it is missing the raw
  string is kept in `published_at_raw` and the timestamp stays null.
- An empty `reviewText` is valid and common. A rating with no words still counts
  toward the average, so it is kept.
- A place with zero reviews produces no rows at all, so it is indistinguishable
  from "no such place". Both are recorded as `no_result` rather than guessed at.
- `ownerReply` is frequently null. Normal, not an error.
