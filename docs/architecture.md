# The stack, and what is actually built

What this system is made of, what runs in production, and what each piece was
built to do. Written to be read by somebody who has never seen the repository.

Companion documents: `runbook.md` is the operational procedure (deploy order,
imports, backups, incident response); `README.md` is how to run it locally.
This one is the map.

Accurate as of migration **0025**, 2026-08-30.

---

## 1. What the product is

A directory of every licensed contractor in Los Angeles County, built from
California's public CSLB licence register, plus a job board homeowners post to.
The product's whole claim is **verification**: the licence is the content. That
single idea explains most of the design decisions below — why source data is
kept in its own tables, why a badge is computed and never asserted, why an
address a contractor typed is labelled differently from an address the register
publishes.

Two sides, mutually exclusive. **An account is a homeowner or a contractor and
can never be both** — enforced in the handlers, in the domain, and by a database
trigger, so a code path that forgets still cannot produce a homeowner who owns a
listing.

Current scale in production:

| | |
|---|---|
| Contractors | 49,774 |
| Licence records / versions | 49,774 / 49,774 |
| Verification checks | 248,871 |
| Contractors with published reviews | 503 |
| Published reviews | 11,248 |
| Scraped reviews held in staging | 25,810 |
| Database size | 479 MB |

---

## 2. The stack

### Backend — Rust

| Piece | Choice | Why |
|---|---|---|
| Language | Rust 2021 | |
| HTTP | **axum 0.8** + tower / tower-http | Trace, timeout, request-id, catch-panic, body limits as middleware |
| Database | **sqlx 0.8** against PostgreSQL 16 | Async, no ORM. Queries are SQL text, so "which query touches a restricted column" stays a `grep` |
| Spatial | **PostGIS 3.6** | `geography` columns, GiST indexes, `ST_DWithin` radius search |
| Search | **pg_trgm** + **unaccent** | A custom `english_unaccent` text-search configuration, immutable so it can back a generated column and an index |
| Passwords | **argon2** | Parameters pinned and asserted by a test |
| Tokens | **jsonwebtoken**, **hmac**, **sha2**, **subtle** | Firebase RS256 verification; constant-time comparison |
| Images | **image 0.25** (jpeg, png, webp) | Decode-and-re-encode, which is how EXIF is discarded |
| HTTP client | **reqwest** with rustls | Geocoding, GCS, Google JWKs. No OpenSSL |
| Serialisation | serde / serde_json | |
| Ids | **uuid v7** | Generated in Rust, never by the database — a DB default would silently hand out v4s and lose time ordering |
| CLI | clap 4 | One binary, subcommands |
| Logging | tracing + tracing-subscriber | JSON in production |

Release profile: `lto = "thin"`, `codegen-units = 1`, `strip = "debuginfo"`.

### Frontend — TypeScript

**Next.js 15** (App Router) · **React 19** · **Tailwind CSS 4** · **zod 4** for
runtime response validation · **react-hook-form** · **framer-motion** ·
**leaflet / react-leaflet** for maps · **@phosphor-icons/react** ·
**firebase** (client SDK, federated sign-in only) · **zustand** ·
**@tanstack/react-query**.

### Infrastructure

Single GCP VM, `us-west2-a`, Ubuntu 22.04.5 LTS, 8 vCPU / 31 GB RAM / 146 GB
disk (16% used). Caddy 2.11 terminates TLS. Node 22. PostgreSQL 16.15 with
PostGIS 3.6.4, on the same box.

---

## 3. Crate layout

Seven crates. The boundaries are load-bearing: **nothing in `cm-domain` writes
SQL**, so the set of queries that can touch a restricted column is exactly the
set of files under `cm-db`.

```
cm-core      1,490  configuration, error taxonomy, id generation, secrets
cm-db        6,253  every SQL query, grouped by the table it owns
cm-auth      3,373  hashing, sessions, cookies, CSRF, rate limits, federated tokens
cm-domain    4,810  business rules — testable with no HTTP
cm-storage     726  object storage and the image normaliser
cm-api       7,205  the HTTP surface: routes, handlers, extractors, middleware
cm-server      960  the binary and its subcommands
```

`cm-db/src/repo/` has one module per table area: `audit`, `claims`,
`contractors`, `geocode`, `job_photos`, `jobs`, `licenses`, `maintenance`,
`messaging`, `oauth`, `passwords`, `profiles`, `rate_limit`, `reference`,
`reviews`, `search`, `sessions`, `users`.

---

## 4. The database

25 migrations, applied in order, checksummed. Editing an applied migration is
rejected — there is a test that tampers with `0001` and asserts the failure.

| # | What it added |
|---|---|
| 0001 | Extensions and the `english_unaccent` text-search configuration |
| 0002 | Reference data: geographic regions, the trade taxonomy |
| 0003 | Accounts and roles |
| 0004 | Password credentials |
| 0005 | Sessions and single-use auth tokens |
| 0006 | Audit log (append-only) |
| 0007 | Rate-limit counters |
| 0008 | Federated identities |
| 0009 | CSLB source data, kept separate from anything we author |
| 0010 | Contractors — our record of a business |
| 0011 | The geocoding queue |
| 0012 | Claims and the evidence behind a verified badge |
| 0013 | Homeowner profiles |
| 0014 | Direct messaging |
| 0015 | Blocking and reporting |
| 0016 | Account type: homeowner or contractor, never both |
| 0017 | Jobs |
| 0018 | Jobs become a nine-field intake form, and gain photos |
| 0019 | The directory publishes the address on the licence |
| 0020 | Where a contractor's supporting data came from |
| 0021 | Facebook as a second federated provider |
| 0022 | Publishing the Google reviews the enrichment load collected |
| 0023 | A link back to the Google listing the reviews came from |
| 0024 | What a contractor owns about their own listing |
| 0025 | The words homeowners use for trades |

### Schema invariants, enforced by tests

`crates/cm-db/tests/migrations.rs` asserts these against a real PostgreSQL, not
a mock — partial unique indexes, CHECK constraints and PostGIS types are exactly
what a mock cannot reproduce, and they are what carries the guarantees. All are
scoped to the `public` schema.

- **Every foreign key is single-column and has an index leading with it.** An
  unindexed FK turns a parent delete into a sequential scan while holding locks.
- **UUID primary keys carry no database default.**
- **Every table has `created_at` and `updated_at`**, with one documented
  exception: `audit_log` is append-only, so `updated_at` would be a field that
  can only ever lie.
- **Exactly four extensions**: `plpgsql`, `postgis`, `pg_trgm`, `unaccent`.
  `citext` is deliberately absent.
- **Status columns are TEXT + CHECK, not native enums**, and each vocabulary is
  pinned to the matching Rust `ALL` array by a test — two hand-written lists
  that would otherwise drift into a 500 in production rather than a compile
  error.

### Source data is never edited

`license_records` holds the CSLB import, versioned in `license_record_versions`,
traced by FK to the `license_import_runs` row it arrived in. `contractors` is
our record, joined to it. The importer writes only source-derived columns, so a
refresh cannot overwrite a claimant's bio — and once a listing is claimed, even
the display name stops being source-derived.

### The staging schema

`staging.*` holds the Google Maps enrichment load — `gmaps_places`,
`gmaps_reviews`, `contractor_place_matches`, `place_match_attempts`,
`scrape_runs`. It sits **outside every invariant above**, deliberately: its
shape is dictated by whatever a third-party actor happens to return.

`cm_app` has no access to it at all — no `USAGE` on the schema, no `SELECT` on
the tables. The only crossing is `tools/gmaps-enrichment/publish.sql`, which
copies vetted rows into product tables. That is why the match-quality gate
cannot be bypassed by application code.

---

## 5. Location privacy

The most subtle rule in the system, and the one most easily broken by an
innocent-looking change.

**No query outside the writer selects `precise_point`.** Every read path returns
`public_point`.

That is held by a behavioural test rather than a static check —
`reads_return_the_published_point_and_never_the_precise_column` writes a precise
point that deliberately differs from the published one, then drives the list,
the map, a text search and a radius search and asserts every one returns the
published coordinates. (The source comment in `repo/contractors.rs` says a test
"greps this crate"; no such grep test exists. The behavioural test is stronger
anyway — it would catch a leak through a join or a view that a grep would
miss — but the comment overstates what is enforced and should be corrected.)

The invariant is *"search reads the same point the map shows"* — **not** "the
published point is coarse". If distance search ran against the precise point
while the map published a ZIP centroid, the radius filter could be binary-
searched to recover the address the centroid was protecting.

Three columns: `precise_point` (geocoded, never published), `public_point` (what
everything reads), `public_point_source` (`exact` / `zip_centroid` / `none`, so
a client can say honestly how precise the pin is).

Since 0019 the directory publishes the exact licence address — it is a public
record and the register publishes the same thing — so the two points usually
coincide. The separation still matters: it is what a `protected` listing relies
on, and it is why search and map can never disagree.

`location::republish()` reads what is already on the row rather than taking a
point from the caller, so it cannot lose one. That property fixed a real bug:
its predecessor called `set_location` with `precise: None`, and `set_location`
writes NULL for a `None` — which would have demoted all 46,018 exact pins on the
next CSLB refresh.

**Since 0024**, an approved claimant may override the displayed address. The pin
follows it (`geocodable_address` prefers the owner's address; an edit
re-enqueues geocoding), the licence address is never edited and stays visible,
and a CHECK makes the owner address whole-or-absent — which is what makes the
per-column `COALESCE` safe from assembling a street from one address and a city
from another.

---

## 6. Authentication

- **Argon2id** password hashing, parameters asserted by a test. Hashing runs off
  the connection pool, so a login storm cannot starve ordinary queries — there
  is a test that holds a two-connection pool under 24 concurrent logins and
  asserts ordinary work still gets a connection.
- **Opaque session tokens** in `__Host-` prefixed cookies: HttpOnly, Secure,
  SameSite, host-bound. Being same-origin with the API is what lets `fetch` send
  them with no credentials plumbing and no CORS anywhere.
- **CSRF** tokens keyed by the session and a server-side pepper, sent in
  `x-cm-csrf` on every non-GET.
- **Rate limits** as named policies with a window and a ceiling, applied per
  account or per client.
- **Federated sign-in** with Google and Facebook via Firebase, verified with
  **no Admin SDK**: RS256 against Google's published JWKs, with the provider
  named by the route rather than trusted from the token. Sign-up carries the
  account type the person chose and **refuses rather than defaulting** to
  homeowner, because a token cannot say which side of a marketplace someone is.
- **Roles are granted, never claimed.** There is no "homeowner" role — homeowner
  is the absence of anything else. `contractor` is granted only when a moderator
  approves a claim.

There is deliberately **no HTTP endpoint that grants a role**. The first admin
comes from `cm-server admin grant-role` over SSH: shell access to the box is a
stronger prerequisite than any check that could be written.

---

## 7. The verified badge

Computed from an approved claim **and** an active CSLB licence. Never asserted
by a request — `PATCH /v1/contractors/{id}` refuses outright if the body so much
as mentions `verified`, rather than ignoring it and teaching the client it
worked.

A claim can be approved while `verified` comes back false, because the licence
is suspended. Both are returned, and a stored `verification_reason` is written
for a person to read: *"CSLB licence 1047382 is suspended as of the last
import"* is an answer; a bare `false` is not.

`cm-verification.timer` re-derives the badge on a schedule, so a licence going
inactive removes it without anyone intervening.

---

## 8. Claims

The only path to becoming a contractor. There is no contractor sign-up: an
account registers, finds the listing the CSLB import already created, and asks
for it.

Three methods exist in the vocabulary (`license_phone_otp`, `license_mail_code`,
`manual_review`) but only **manual review** is offered — the other two have no
OTP delivery, no mail delivery and no code-submission endpoint, so showing a
code field would be building an input that can never receive a code.

Partial unique indexes carry the real safety: one pending claim per pair, one
approved claim per contractor, one per user. Two simultaneous approvals produce
exactly one owner — the second changes no rows and is told so.

`GET /v1/admin/claims` returns a contractor id, a user id and the claimant's
evidence blob. It returns **no business name and no claimant email**, and there
is no endpoint to look either up, so the evidence the claimant typed is the
entire basis for the decision.

---

## 9. Jobs

A nine-field intake form, eight required, each with a deliberate escape hatch so
"I don't know" is an answer a person can give rather than a blank they leave.

The schema's trick: **absence is the escape hatch.** `trade_id IS NULL` means
"Other / not listed"; both budget columns NULL means "I'm not sure". That is
only sound because the API forbids a field to simply be missing — a CHECK makes
the half-filled budget state unrepresentable.

### Photos, and why they are re-encoded

`jobs` has no address column and no precise point. That is the entire privacy
argument for the table. **A photo of a house carries the GPS coordinates of that
house in its EXIF** — storing an upload as-is would publish the exact address of
every job, worse than the column we refused to create, because nobody would
think to look in an image for it.

So every image is decoded and re-encoded, which discards all metadata *by
construction* rather than by remembering to strip particular tags. EXIF
orientation is read and applied first, then thrown away, or every phone photo
lands sideways. The same pass rejects anything that is not really an image and
caps decoded pixels against a decompression bomb. Long edge 2000px, JPEG q82.

`Store::put` takes a `Normalised`, not bytes, so no call site can skip the pass.
The store is an enum — `Gcs` in production, `Memory` in tests — and
`check-config` refuses to start a production server with no bucket configured,
so the memory store can never be a silent downgrade. Bucket:
`cm-job-photos-6b1e669f`, objects public-read, deleted outright when a job is
cancelled.

Contractor profile photos (0024) go through the identical pass.

---

## 10. Reviews

~25,000 Google Maps reviews were collected for contractors in the directory.
They live in `staging` and are copied into product tables by a re-runnable
promotion step.

**The gate is exact-name matches only, and that number was measured rather than
guessed.** A first pass admitted anything with 0.55 name similarity, yielding
821 contractors. Scoring a random sample of thirty published matches by hand
returned **fifteen right, fifteen wrong** — and the errors were not noise. They
were initialisms colliding with other initialisms, which the similarity metric
scores generously because so few characters are in play:

```
W P CONSTRUCTION       →  W F Construction               0.94
T L A HEATING & AIR    →  VT Heating & Air Conditioning  0.90
LBFC GENERAL CONSTR.   →  DM Construction                0.80
GARAY'S ELECTRIC       →  Gary's Auto Electric           0.70
```

Because those score high, **no floor below exact separates them**. The published
set is 503 contractors rather than 821 of which ~410 would have carried a
stranger's reviews. The category signal that should have caught the worst cases
was unavailable: the actor never returned `placeCategory`, so it is NULL on
every place.

Two consequences visible in the product:

- **No dates.** The scraper never received an absolute timestamp, only Google's
  relative phrasing. `relative_age` stores "a year ago" verbatim; parsing it
  would invent precision the source does not have, and sorting by it would
  produce an order that looks chronological and is not. Ordering is Google's own
  "most relevant" sequence.
- **The count is Google's, the list is a sample.** The scrape caps at 200 per
  place and the API returns at most 30, so the profile says "showing 30 of
  1,709" and links out, rather than letting the mismatch read as a bug.

Every review is attributed. This site hosts none of its own and has no review
form.

---

## 11. Search

Keyset pagination on `(display_name, id)`, never `OFFSET` — which both scans
what it skips and duplicates rows when the data shifts between pages.

**One shared `WHERE` clause** backs the list and the map, so they can never
disagree about what matches. A map showing pins the list omits is a bug report
nobody can reproduce.

Full-text over a generated `search_doc` column using `english_unaccent`, with a
trigram fallback so a near-miss on a business name still finds it. The map has a
hard cap of 500 points and reports `truncated: true` rather than returning a
silently partial map.

The fallback is **word** similarity (`<%`), not whole-string similarity (`%`),
and the distinction is the whole feature. `%` scores the query against the
entire column: "ibara" against "Ibarra & Daughters Construction" scores 0.161
against a 0.3 threshold and does not match — nor does any other typo in any
other multi-word business name, which is nearly all of them. The fallback was
advertised and did not work. `<%` scores against the closest word instead, gives
0.667 on that pair, and serves from the same `contractors_name_trgm` GIN index.

### Relevance is measured, not asserted

`cm-domain/tests/search_quality.rs` scores a graded golden set —
`tests/fixtures/search_golden.jsonl`, 23 queries with hand-labelled relevance
judgements — against a 16-business corpus, and fails the build if mean NDCG@10
or Recall@20 drops below a pinned floor. The floors are measurements: they were
read off a run and are raised in the same commit as the change that earns them.
It is what makes "did that ranking change help?" a number rather than an
argument, and it is how the `%`/`<%` defect above was found and then shown
fixed (0.468 → 0.607 NDCG@10, 0.471 → 0.623 Recall@20).

### The taxonomy, and the words people use

Two layers sit between "what a homeowner typed" and "which licence class".

**The classification set.** `trades` held six of the ~80 CSLB classifications,
because v1 only filtered on a handful. The importer maps a licence through that
table and drops what it cannot match, so a licence in any other class arrived
with **no trade at all**: six classifications cover 61% of the 311,732
licence-classification pairs in the real register, and 27% of a 3,000-row sample
matched no trade filter that existed. It is now 75, covering 98.9%, and the
importer logs what it still drops rather than discarding it silently — currently
`HAZ` and `ASB`, which are certifications rather than classifications, and a
handful of D-codes CSLB no longer publishes names for.

`seed-trades` also re-derives every contractor's trades, because nothing else
can: a migration runs *before* it in the deploy order, and re-importing the same
file short-circuits on unchanged licences and never reaches the trade-writing
line.

Not every trade is offered as a filter. `active` marks the 30 a homeowner would
plausibly pick; the other 45 are matched on import and reachable by search, but
kept out of a picker that would otherwise open with "Air and Water Balancing".

**The vocabulary.** Expanding the taxonomy fixed the `?trade=` filter and did
nothing for the search box, because free text is matched against a business name
and a bio and no business is called "hvac". `trade_aliases` maps how a person
describes a problem — "water heater", "rewire", "adu", "leaking pipe" — to how a
licence is classified, and a query resolves through it before the search runs. A
table rather than a model: the mapping is small and knowable, and a wrong entry
is one statement to fix.

Alias matching uses a stricter similarity bar than name matching (0.70 against
0.50), and the two are separate for a measured reason: at the name threshold
"tree removal" matched the alias "junk removal" at 0.615 and returned janitorial
companies for tree work. A short curated phrase is dominated by one shared common
word in a way a business name is not.

Together these took the golden set from 0.644/0.667 to **0.971 NDCG@10 and 1.000
Recall@20**. What remains below 1.0 is a ranking problem, not a retrieval one:
queries matching two businesses tie on `ts_rank` and fall back to alphabetical
order, which puts a 3.8-star unclaimed listing above a 4.5-star verified one.

---

## 12. The HTTP surface

31 routes under `/v1`. Public: contractors (list, map, detail), trades, regions,
jobs. Authenticated: auth (register, login, logout, logout-all, password,
Google, Facebook, link), me, claims, the claimant's listing and photo, jobs and
job photos, messaging, blocks, reports. Moderation: `/v1/admin/claims`,
`decide`, `reports`.

Body limits travel with the routes that need them, merged as their own router
rather than applied globally — 12 MB on job photos, 8 MB on a profile photo.

---

## 13. Operator commands

One binary, subcommands, because on a single box several binaries is several
things to keep in step:

```
serve                    Serve the HTTP API
migrate                  Apply outstanding migrations, then exit
check-config             Validate the environment and print resolved config
import-cslb              Import an operator-supplied CSLB licence file
load-regions             Load ZIP-code centroids
seed-trades              Insert the canonical trade set (idempotent)
recompute-verification   Re-derive the badge for every contractor
prune                    Delete what nothing needs, in bounded batches
geocode-worker           Resolve queued addresses into coordinates
admin grant-role         Grant a role to an account
admin revoke-role        Remove a role
```

**`serve` refuses to start against a schema older than the binary.** This is why
the deploy order is law: **build → migrate → restart**. Getting it backwards
produces a service that will not start.

---

## 14. Production

```
                    Caddy 2.11  :443  (TLS, zstd/gzip)
                         │
        handle_path /api/*        handle /*
                         │              │
              cm-server :8080     cm-web :3000
                   (Rust)          (Next.js)
                         │
                  PostgreSQL 16 + PostGIS 3.6  :5432
```

Caddy's `handle_path /api/*` strips the prefix, so the browser sees
`/api/v1/...` and the service sees `/v1/...`; the service knows nothing about
its mount point. `X-Forwarded-For` is **replaced, not appended** — the service
trusts only the last entry and only from a loopback peer, so it is handed
exactly one value the client cannot influence.

Sharing an origin is what makes `__Host-` cookies work and why there is no CORS
anywhere in the codebase.

### Units

| Unit | Role |
|---|---|
| `cm-server` | The API |
| `cm-web` | Next.js front end |
| `cm-geocode-worker` | Drains the geocode queue |
| `cm-verification.timer` | Re-derives the badge on a schedule |
| `caddy` | TLS and routing |
| `postgresql` | Database |

All active and enabled.

### Hardening

`cm-server` runs as an unprivileged `cm` user under a systemd sandbox:
`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
`PrivateDevices`, `ProtectKernelTunables`, `ProtectKernelModules`,
`ProtectControlGroups`, `RestrictNamespaces`, `LockPersonality`,
`MemoryDenyWriteExecute`, `SystemCallArchitectures=native`,
`RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`, `MemoryMax=1G`.

### Database roles

Two, and the split is deliberate:

- **`cm_migrate`** owns the schema and runs migrations. Default privileges grant
  `cm_app` access to what it creates, which is why a new table is readable
  without a manual GRANT — and why migrations must run as this role.
- **`cm_app`** runs the service. It has `INSERT/SELECT/UPDATE` on `public` and
  **no access to `staging` whatsoever**.

Credentials live at `/etc/cm-backend/env` (service) and
`~/.config/cm-migrate.env` (migrations, 0600).

---

## 15. Testing

342 test functions. Every database test runs against a **real PostgreSQL 16 with
PostGIS** — there is no mocked database anywhere in the suite, on purpose.

| Area | What is covered |
|---|---|
| `cm-db/tests/migrations.rs` | Schema invariants, vocabularies, constraints, cascades |
| `cm-api/tests/security.rs` | CSRF, cookies, headers, authorisation boundaries |
| `cm-api/tests/auth.rs`, `google.rs` | Password and federated flows |
| `cm-auth/tests/credential_race.rs` | Concurrent logins, password races, pool starvation |
| `cm-auth/tests/firebase.rs` | Token verification against JWKs |
| `cm-api/tests/claims.rs` | Claim lifecycle, simultaneous approvals, badge derivation |
| `cm-api/tests/jobs.rs` | Intake validation, photo upload, EXIF stripping |
| `cm-api/tests/directory.rs` | Search, pagination, location projection |
| `cm-domain/tests/import.rs` | CSLB import, versioning, idempotence |
| `cm-domain/tests/search_quality.rs` | Ranking quality: NDCG@10 and Recall@20 against a graded golden set |
| `cm-server/tests/schema_gate.rs` | `serve` refusing a stale schema |

The one that matters most: **`the_stored_image_carries_no_metadata`** builds a
JPEG with GPS EXIF, runs it through the normaliser, and asserts no APP1 segment
and no GPS bytes survive.

> **Note on running tests remotely.** The suite is latency-sensitive.
> `queued_hashing_does_not_occupy_database_connections` gives a 750 ms acquire
> timeout to a two-connection pool under 24 concurrent logins; over an SSH
> tunnel at ~59 ms round-trip it fails every time, and passes against a local
> database. Run the suite on the box, not through a tunnel.

---

## 16. Data pipelines

### CSLB import

`cm-server import-cslb --file … --source cslb_master_list --county "LOS ANGELES"`

Content-hashed per record, versioned, idempotent — an unchanged row is counted
as unchanged rather than rewritten. Every licence record is traceable by FK to
the import run it arrived in. A file is imported once per source, enforced by a
partial unique index on `(source, source_file_sha256) WHERE status = 'succeeded'`.

### Geocoding

An address change enqueues a job keyed by a digest of the address, so repeated
imports of the same address do not requeue. `cm-geocode-worker` claims batches
with `SKIP LOCKED` — which is what makes a second worker useful rather than
merely blocked — and marks them in the same transaction. `requeue_stalled`
recovers jobs left `in_progress` by a crash; without it, a crash would leak
queue capacity permanently.

### Google Maps enrichment (`tools/gmaps-enrichment/`)

Python, 81 tests (49 matching, 17 Apify client, 15 pipeline), writes only to
`staging`. Notable engineering:

- **Hash-based sharding** so several workers run concurrently without a claim
  table — disjoint by construction. Verified against the real table before
  launch: all 49,774 contractors in exactly one shard, buckets 19.8–20.3%.
- **Cost tuning from measurement.** The usage breakdown showed residential proxy
  at 84% of spend; disabling it and retuning concurrency and page size took the
  yield from 177 to 827 reviews per dollar in testing.
- **Derived identifiers.** The actor returns neither `placeId` nor `reviewId`.
  Google's own feature id is extracted from the place URL (a real identifier,
  stable across runs); review ids are digests of the fields that do not drift —
  deliberately excluding `publishedAt`, which changes from "4 months ago" to "5
  months ago" on its own and would mint a new id on every run.

Known limitation, documented in the script: `--max-spend` reads the ledger once
at startup and each worker adds only its own spend, so with N workers the
effective cap is roughly N×. The run that produced the current data ended by
hitting the provider's account ceiling instead.

---

## 17. Deliberate omissions

Stated so they read as decisions rather than gaps.

| Not built | Why |
|---|---|
| Email verification, password reset | No mail path yet. The UI says so rather than showing a badge implying an address is confirmed |
| Account linking UI | The `/v1/auth/link/*` endpoints exist and nothing reaches them; deferred |
| Messaging UI | Endpoints and schema exist; deferred |
| Phone OTP / mail-code claims | In the vocabulary, no delivery mechanism. Only manual review is offered |
| Review writing | This site hosts no reviews of its own |
| Off-host backups | Open item |
| CI | Open item |

---

## 18. Open items

1. **One moderator account exists** (`contractormarketplace.co@gmail.com`).
   Nobody else can approve a claim.
2. **Reviews are hard to find.** 503 of 49,774 listings carry them (~1%), and
   the directory sorts alphabetically, so the first page shows none. A
   rated-first sort or a has-reviews filter would fix it.
3. **Off-host backup** is not configured.
4. **CI** does not run on GitHub.
5. **Domain cutover** to `contractorsmarketplace.co`: DNS, Caddy TLS, and
   `CM_SITE_ORIGIN` — `__Host-` cookies are origin-bound, so this is not just a
   DNS change.
6. **`APIFY_TOKEN` should be rotated**; it was handled in plaintext during the
   enrichment work.
