# Search and queries

Everything the two boards do: how a query becomes results, how results are
ordered, how pages are walked, and how any of it is known to be working.

`architecture.md` is the map of the whole system; this is the part of it that
answers questions. Written to be read by somebody who has never seen the
repository, and to be useful to somebody about to change a ranking.

Accurate as of migration **0028**.

---

## 1. The two surfaces

| | Homeowner | Contractor |
|---|---|---|
| Board | `GET /v1/contractors` | `GET /v1/jobs` |
| Map | `GET /v1/contractors/map` | `GET /v1/jobs/map` |
| Personalised | — | `GET /v1/me/jobs/feed` |
| Typeahead | `GET /v1/suggest` | `GET /v1/suggest` |

Both boards are public and take no session. The job board's module says so in
its first paragraph and means it literally: there is no session extractor on it,
so there is no branch that could serve the wrong caller the wrong shape. The
personalised feed is a separate authenticated route for exactly that reason.

Each board shares one `PREDICATE` constant with its map, so the two can never
disagree about which rows exist. A map showing pins the list omits is a bug
report nobody can reproduce.

---

## 2. How a query becomes results

Four ways a contractor can match a piece of text, in the order of how much each
one tells you.

**The text matched.** Postgres full text over a generated `search_doc` column
using a custom `english_unaccent` configuration, so "Íbarra" and "Ibarra" are
the same word. The document holds the business name, the bio and the postal
code, with the name at weight A and prose at B.

**It is that kind of contractor.** The query is resolved against a vocabulary of
the words people actually use — "water heater", "rewire", "adu", "hvac" — and
those resolve to CSLB licence classifications. No business is called "hvac", so
without this the whole class of problem-shaped queries returns nothing however
good the text matching is.

**The name is close.** Trigram word similarity, for a typo.

**Nothing.** Which is a valid answer and is tested: `zzzzznotarealbusiness`
returns no rows, and that assertion is the guard against a change that quietly
makes everything match.

### The vocabulary

`trade_aliases` maps how a person describes a problem to how a licence is
classified. A table rather than a model, deliberately: the mapping is small and
knowable, an operator can correct a wrong one with a single statement, and there
is nothing to retrain. It is seeded from a constant in `reference.rs` and
rewritten on every `seed-trades`, so deleting an alias from the constant
actually removes it.

Matching runs in **two directions**, and they answer different questions:

- `word_similarity(query, alias) >= 0.70` — *is what they typed roughly this
  alias?* This is what handles a typo and what a one-word query needs.
- `word_similarity(alias, query) >= 0.90` — *does this alias appear inside what
  they typed?* This is what a sentence needs. Nobody types "roofer"; they type
  "cheapest roofer" or "my house needs rewiring", and the sentence is nothing
  like the short phrase inside it.

Both thresholds were measured rather than chosen. The forward one is stricter
than the 0.50 used for business names because a short curated phrase is
dominated by one shared common word in a way a business name is not — at 0.50,
"tree removal" matched the alias "junk removal" at 0.615 and returned janitorial
companies for tree work. The containment threshold is strict because containment
should mean containment: the alias genuinely present in a sentence scores 1.000,
and the best wrong one reaches 0.250.

### The taxonomy

`trades` holds 75 CSLB classifications, covering 98.9% of the 311,732
licence-classification pairs in the real register.

It held six. That was not a small gap: `import.rs` maps a licence to a trade
through this table and drops what it cannot match, so a licence in any other
class arrived carrying **no trade at all**. On a 3,000-row slice of the real
register, 803 contractors — 27% — matched no trade filter that existed. C-20,
heating and air conditioning, was the fifth largest class in that slice, ahead
of two that were mapped.

What is still left out is left out on purpose. `ASB` and `HAZ` are
certifications rather than classifications — a contractor is not "a HAZ" the way
they are a plumber. `C-49` and a few D-codes appear in the register but not in
CSLB's current published list, and a guessed name is a wrong label shown to a
homeowner rather than an absent one. The importer counts what it drops and says
so, instead of discarding it silently.

**Featured versus mappable.** `active` marks the 30 trades the picker offers.
Every one of the 75 is matched on import and reachable by search. The importer
reads `all_trades_for_import`, not `all_trades`, because what a homeowner is
offered in a dropdown and what a licence can be classified as are different
questions — otherwise a C-11 licence would import untraded purely because
"Elevator" is not worth a dropdown row.

**Re-deriving.** `seed-trades` rewrites every contractor's trades after seeding.
This cannot be a migration and cannot wait for the next import: a migration runs
*before* `seed-trades` in the deploy order and would derive against the previous
release's taxonomy, and `import::flush` short-circuits on an unchanged licence,
so re-importing the same file never reaches the trade-writing line.

---

## 3. Ranking

### Contractors

```
rank = (text matched      ? 1.00 : 0)
     + (routed to a trade ? 0.75 : 0)
     + (name is close     ? 0.50 : 0)
     + ts_rank_cd(...)                 -- separates text matches from each other
     + 0.5 × quality_score             -- orders equals
```

The order of the first three is the design, and the middle one was earned rather
than assumed: "solar" returned "Polar Air Heating & Cooling" above an actual
solar contractor until a trade match was scored above a fuzzy name match. Being
that kind of contractor is a fact about the licence; having a name one letter
away from the word typed is a coincidence about spelling.

With no query the first four terms are zero and the whole thing is quality,
which is what turns browsing from alphabetical into best-first.

**`quality_score`** is one number per contractor, derived nightly beside the
verified badge from the same source data:

- a Bayesian-adjusted rating, `(C·m + r·n)/(C + n)` with `C = 10`, so one
  five-star review does not outrank a 4.7 across three hundred;
- `ln(1 + reviews)`, saturating at 500, so volume alone cannot dominate;
- boosts for verified, for claimed, and for a page somebody filled in.

The formula lives in `cm-domain/src/quality.rs` as a pure function with unit
tests, not inside an `ORDER BY`. Weights are configuration (`CM_RANK_W_*`),
because ranking is tuned by looking at results and a weight that needs a
redeploy does not get tuned.

Distance is deliberately **not** a term. Everything inside a radius filter is
already near enough, and blending distance in would mean a slightly closer,
slightly worse contractor outranks a better one for reasons the visitor cannot
see. `sort=distance` exists and says plainly what it does.

### Jobs

The board defaults to newest, because a job board is a queue before it is a
search result. `sort=best`, `budget` and `distance` are available.

`GET /v1/me/jobs/feed` orders by fit for the contractor asking:

```
fit = (my trade ? 1.00 : 0)
    + 0.60 × exp(−distance / 40 km)
    + 0.50 × exp(−age / 14 days)
    − 0.25 × ln(1 + other contractors already in conversation)
```

The last term is what spreads leads across the supply side rather than piling
every contractor onto the same posting: a job with nine replies is worth less to
the tenth contractor than one with none.

**These weights are reasoned, not measured**, and that is their honest state.
Nothing here has been tuned against behaviour, because until `search_events`
shipped there was no behaviour recorded. See §7.

---

## 4. Pagination

Keyset, never `OFFSET`, which both scans what it skips and duplicates rows when
the data shifts between pages.

The cursor carries **the key its ordering leads with**, not just the stable
tie-break. It did not, and that was a real defect: under `sort=distance`, page
two filtered on a column it was not ordered by and silently dropped rows — which
the front end worked around by refusing to paginate scored sorts at all and
capping them at fifty results.

Two things about the implementation are easy to get wrong and are worth reading
before touching it:

**The comparison cannot be row-wise.** It is written as "past the key, or level
with it and past the tie-break", because `(a, b, c) < (x, y, z)` applies a
single direction to every column while the ordering is `key DESC, name ASC`. The
row-wise form silently means `name DESC` and excludes everything alphabetically
after the cursor. It returns a short page that looks like a complete one.

**A bind the statement never mentions is not harmless.** The tail numbers itself
from what precedes it — the predicate's slots, then any the ordering itself
consumes, then the key slot when there is one — and binds the key only when the
statement refers to it. Postgres counts the placeholders it can see and refuses
the extra.

A cursor from an earlier release has fewer fields than the current shape, fails
the shape check, and comes back as a 400 saying the page cursor is not valid.
That is the right answer: accepting it would mean resuming a scored ordering
from a name.

---

## 5. Facets

The job board returns counts alongside results, from **one `GROUPING SETS`
query under the same predicate**. Counts that disagree with the list beside them
are worse than no counts, because they are read as the list being wrong.

`GROUPING()` is load-bearing there. `trade_id IS NULL` is the board's "Other /
not listed" escape hatch, so the trade set contains a NULL-slug row identical in
shape to the grand-total row from the empty set — without the flags, one
silently overwrites the other.

Each count is taken with every current filter applied, including the facet's
own. The number beside "Roofing" is how many roofing jobs match, not how many
there would be if roofing were selected instead. That is the honest reading of
"what is in front of me", and it is why selecting a facet never surprises.

The `total` is what the board never had: it could only count the rows it had
loaded, so it said "20+ jobs" for four hundred.

---

## 6. Geography

Every read path publishes `public_point` and never `precise_point`. This is the
most fragile invariant in the system: if distance search ran against the precise
point while the map published a centroid, the radius filter could be
binary-searched to recover the address the centroid was protecting. A
behavioural test performs exactly that attack against every read path.

### Coverage, not proximity

A location filter asks **"who travels here"**, not "who is registered here".
Those give different answers exactly where it matters: a roofer eight miles out
with a fifty-mile radius covers you; a sole trader two miles away who only works
their own neighbourhood does not.

Every contractor has `service_radius_m`, defaulting to **25 miles** — including
the unclaimed majority, which is what makes the question answerable for the
whole register rather than for the handful of claimants who have filled a form
in. Claimants can change it, and can additionally name specific ZIPs in
`contractor_service_areas`.

Said literally the test is `ST_DWithin(c.public_point, me, c.service_radius_m)`,
a per-row distance no spatial index can answer. It is split in two instead:

- contractors **at the default** are matched with a constant radius, which the
  GiST index serves directly
- contractors **who changed it**, in either direction, plus anyone who named
  this place, are resolved by one small query ahead of the statement and arrive
  as an id array

The equality on `service_radius_m` in the first branch is what makes the split
sound. Without it, somebody who narrowed their radius to five miles would still
be matched at twenty by the constant — the filter would silently only ever
widen.

**The searcher's own `radius_m` narrows and does not select.** It is a separate
`AND`, absent by default, for somebody who wants only the closest of the
contractors who already cover them. Defaulting it — which the API used to do,
at 25 km — silently hid everyone who travels further, which is most of the point
of coverage.

**`zip=` is the older, narrower filter** and matches a contractor's own postal
code exactly. It answers "who is registered in this ZIP", which is rarely the
question, and it is no longer reachable from the directory UI.

**ZIP centroids.** `deploy/data/zcta_ca.csv` carries all 1,763 California ZCTAs
from the 2020 Census gazetteer. It carried 25. Measured on the real register,
that took ZIP coverage from 25 of 340 to 271 and located contractors from 9% to
96%.

The remaining 69 ZIPs have **no ZCTA anywhere in the United States** — they are
PO-box and business-only ZIPs (90209, 90607–90609, 90733) and the Census only
draws a ZCTA around a populated area. Contractors there are located by geocoding
a street address instead. A job posted in one has no location at all, because
jobs carry no address by design. Watch the unlocated count.

The gazetteer publishes no names, so a bulk load carries the code in the name
column. `upsert_zcta` treats a name equal to the code as a placeholder and keeps
whatever is already there — otherwise loading the file turns "Silver Lake" into
"90026" for every ZIP anyone had curated.

**Service areas.** A contractor matches a place either by being in it or by
saying they serve it. `contractor_service_areas` holds two kinds of row — a
named ZIP, or a travel radius from the contractor's own point — and both are
checked by the shared predicate, so the list and the map agree.

It widens rather than narrows: a listing already inside the search radius still
matches whether or not it has declared anything. That matters because service
areas are set by a claimant and the great majority of listings are unclaimed.

Named areas are matched against `regions.approx_radius_m`, the radius of a
circle with the same land area as the ZIP, derived from a figure already in the
Census gazetteer. It stands in for `regions.boundary`, which has been NULL since
0002 and needs half a gigabyte of TIGER/Line shapefiles and a loader that does
not exist. The approximation over-covers at the corners of a ZIP and
under-covers along a long thin one; for deciding whether somebody serves an area
that is a reasonable trade, and the polygon column is still there for when it is
not.

**Viewport search.** `bbox` narrows both the list and the map. The front end
reports the viewport after the person stops moving and offers to search it,
rather than refetching on every pan. Auto-fitting the map to its results is
switched off while the map is driving the search: refetching produces new pins,
refitting to them moves the map, and moving the map refetches. The map either
follows the results or chooses them.

---

## 7. How any of this is known to work

Two instruments, answering different questions.

### The golden set

`cm-domain/tests/search_quality.rs` scores 33 hand-labelled queries against a
17-business corpus with NDCG@10 and Recall@20, and fails the build below a
pinned floor. The floors are measurements: they are read off a run and raised in
the same commit as the change that earns them.

The journey:

| | NDCG@10 | Recall@20 |
|---|---|---|
| At the start | 0.468 | 0.471 |
| Word-similarity operator | 0.607 | 0.623 |
| Measured similarity threshold | 0.644 | 0.667 |
| Queries routed through the trade vocabulary | 0.971 | 1.000 |
| Blended ranking | 1.000 | 1.000 |
| **Ten harder queries added** | **0.836** | **0.826** |
| Alias containment direction | 0.939 | 0.939 |
| Two fixture corrections | **1.000** | **1.000** |

It is saturated again, and will need growing again. At 1.000 it can only detect
regression, and "no worse" is not "no better".

### Behaviour

`search_events` records one row per result shown, with its position, so
click-through rate by position is answerable. A golden set is one person's
opinion written down once; this is everybody's behaviour recorded continuously.
A ranking whose rate is flat across positions is carrying no information
whatever the judgements say.

It holds no query text, only whether there was one. `router.rs` already keeps
query strings out of its spans because they carry what somebody typed — often
their own name or address — and a table kept for months is a worse place for
that than a log line that rotates.

Writing is best-effort by construction: every call site drops the result. A
missing impression is a gap in an analysis; a search that 500s because logging
failed is an outage.

---

## 8. Typeahead

`GET /v1/suggest?q=` answers the three things somebody can be reaching for — a
kind of work, a place, or one business — and labels each, so the client turns a
choice into the right filter instead of inferring it from the text. Trades and
places rank above businesses, because choosing one narrows a search rather than
ending it.

Assembled per request from the tables that already exist rather than kept in a
materialised index. At this size the union costs single-digit milliseconds and
is always current; a copy would need rebuilding on every import, every claim and
every trade edit, and the failure mode of forgetting is a suggestion list that
quietly describes last week's directory.

Public and fired on every keystroke, so it carries a named rate limit per
address, enforced before the query runs. Queries shorter than two characters
are answered with an empty list rather than an error — the client asks as the
box fills, and the first keystroke is not a mistake.

---

## 9. Measured latency

Release build, on the production box against 49,774 real listings, warm pool.

| | p95 |
|---|---|
| `/v1/contractors` (browse) | 7 ms |
| `/v1/contractors?sort=rating` | 7 ms |
| `/v1/contractors?q=ibarra` (few matches) | 12 ms |
| `/v1/suggest?q=elec` | 51 ms |
| `/v1/contractors?lat=…&lon=…` (coverage, Glendale) | 181 ms |
| `/v1/contractors/map?lat=…&lon=…` | 191 ms |
| `/v1/contractors?lat=…&lon=…` (coverage, downtown LA) | 229 ms |
| `/v1/contractors?q=water+heater` (routes to a trade) | 321 ms |

Two outliers, both explainable, both the price of answering the right question.

**Trade-routed queries** match every contractor holding that licence class —
thousands of rows — and ranking cannot order what it has not scored. A query
with few matches is 12 ms. The lever, if it is ever needed, is capping
candidates for high-cardinality routes, not the ranking.

**Coverage searches in dense areas** match most of the register, because that is
what the answer is: 25 miles of downtown Los Angeles genuinely covers 41,820 of
the 49,774 listings. The scan is index-served and takes 86 ms; the rest is
ranking a set that large. Note this is not comparable to the older proximity
number it replaced — the previous 169 ms searched a 25 km circle and returned a
narrower, wrong answer, where this covers 40 km and returns the right one over
2.6× the area.

The remaining levers, in order of value: cap candidates before ranking in dense
areas, or precompute a coarse coverage grid. Neither is worth doing until the
click data from §7 says the ranking of a 40,000-row result set matters.

### Five findings about Postgres worth keeping

Each of these looked like the opposite before it was measured.

**`lower(col) LIKE` cannot use a trigram index.** The index is on the bare
column. Wrapping it in `lower()` means a sequential scan; `ILIKE` on the column
itself uses the index.

**Prefix `OR` contains against a trigram index is the same work twice.** To a
trigram index, `plumb%` and `%plumb%` extract identical trigrams and return
identical candidates. Dropping the prefix from the `WHERE` and keeping it only
in the ranking took suggest from 83 ms to 8 ms and lost no results.

**An index cannot serve a computed ordering.** The default browse sorted by
`0.5 × quality_score` and did a sequential scan over every contractor with a
top-N sort. With no query that is the same order as the bare column, and sorting
by the column instead took it from 51 ms to 1.4 ms.

**The obvious optimisation was a 25% regression.** Hoisting a correlated
`EXISTS` into a lateral join to compute it once measured five times faster in
isolation and slower through the real endpoint: the planner can short-circuit an
`EXISTS` per row and cannot skip a join it has already built. The isolated
benchmark inlined a scalar subquery that the shipped query passes as a
parameter, and that one difference changed the plan.

**A subquery on the other side of an `OR` costs the index on the first side.**
Service-area matching first shipped as
`ST_DWithin(...) OR EXISTS (SELECT ... WHERE sa.contractor_id = c.id ...)`,
which is a faithful reading of "in the area, or says they serve it". Because a
correlated subquery cannot be answered from a GiST index, the planner stopped
using `contractors_public_point_gix` for the spatial half too and sequentially
scanned all 49,774 rows: **478 ms against 94 ms**, measured on production data
for a 25 km search. Resolving the service-area half first — one small query over
`contractor_service_areas`, whose result is passed in as an id array — restores
the index. The `IS NOT NULL` guard around the array matters as much as the
rewrite: without it the `= ANY(NULL)` still costs a scan.

The general shape: an `OR` is only as index-servable as its *least* servable
branch. Two branches that each have an index still need splitting — either into
a pre-query, as here, or into a `UNION`.

---

## 10. Deliberately not built

**Semantic search.** Held behind a gate — ship only on a measured gain over what
is here. Ten materially harder queries were added to the golden set to find that
gain: whole sentences, symptoms rather than services, regional slang, a
misspelled trade word. They found a gap of 0.836, and it turned out to be a
missing string comparison rather than a missing model. Everything they ask for
is answered by a table of words and two similarity directions, which leaves
nothing for a vector index to close and nothing to justify the extension,
embedding pipeline, provider dependency and CI image that would arrive with it.

**Saved searches and job alerts.** Built, once the mail path existed. Filters
are typed columns in `saved_searches`, matched against new jobs by the same
clauses as the predicate above; `cm-server job-alerts` renders one weekly digest
per user into the email outbox. See §16a of `docs/architecture.md`.

**A search cluster.** No Elasticsearch, no Redis, no separate search service. At
50,000 contractors, Postgres with the indexes above answers every query here at
the latencies in §9. The exit ramps are known and nothing above forecloses them.

**Boundary polygons, and the containment search they enable.** Considered
directly against how LoopNet does it — search a place, get its real outline
drawn on the map, get an exact count of what falls inside — and declined on
2026-08-31.

`regions.boundary` is a geography column with a live GiST index and zero rows,
and that is now a deliberate state rather than an unfinished one. Filling it
means TIGER/Line ZCTA and Place shapefiles, about half a gigabyte, and a loader.
Every area here stays a circle: a centroid plus `approx_radius_m`, the
equal-area radius from the gazetteer.

**What the circle costs.** A ZIP is not a circle. 91504 is a long wedge running
along the I-5 with a notch bitten out where it meets the Verdugo foothills; an
equal-area circle over it spills north into empty hillside and falls short at
both narrow ends. For "does this contractor plausibly serve this area" that is a
fair trade. It would not survive being drawn on a map next to its own results,
which is exactly why the map is not drawing it.

**The reason it is declined is semantic, not cost.** LoopNet lists properties,
and a property has one fixed location, so "inside the boundary" is the whole
question. A contractor travels. The directory answers "who serves this point",
and a contractor in Glendale who covers Burbank is a correct result that a
containment query would drop. Precision on the outline buys little when the
outline is not what decides the answer.

Revisit if the map starts drawing areas, if a region service area needs to be
exactly right at its edge, or if "contractors based in this place" becomes a
browse people ask for alongside coverage. The column and its index are already
there for it.

---

## 11. Open

- **ZIP boundaries** are still approximated by an equal-area circle.
  `regions.boundary` remains NULL; loading TIGER/Line polygons would make
  service-area matching exact.
- **The golden set is saturated** at 1.000 and needs harder cases again before
  it can steer another ranking change.
- **The feed has no interface.** `/v1/me/jobs/feed` is built and tested; the job
  board is a public page that mounts no current-user provider, so nothing
  reaches it yet.
- **The ranking weights are unvalidated.** They stay that way until
  `search_events` has enough in it to check them against.
- **`cm-frontend` has no CI.** The `lint`, `typecheck` and `test` scripts exist;
  nothing runs them automatically.
