# Handover

What this backend does, what it deliberately does not, and what is a placeholder.

## What it does

**Accounts.** Password registration and login with Argon2id (19 MiB, t=2, p=1).
Opaque 32-byte session tokens stored only as SHA-256 digests, delivered in
`__Host-` cookies. Logout, logout-everywhere, password change that rotates the
current session and revokes the rest, roles, and an audit trail. Google sign-in
via Firebase, where Firebase verifies nothing on our behalf beyond issuing a
token we then check ourselves.

**Account type.** Every account is a **homeowner** or a **contractor**, chosen
at registration and never changed. This is `users.account_type`, not a role —
roles are granted and additive, and `Role::Contractor` still means a moderator
approved a claim. The two are related but distinct: only a contractor *account*
may open a claim, and only an approved claim grants the contractor *role*.

The rule is enforced in three places, deliberately. The handlers refuse the
wrong side with a 403 (claiming, starting a conversation, holding a homeowner
profile). Two database triggers refuse it again, so a code path that forgets
the check cannot record a homeowner as a claimant or give one a homeowner
profile. And the front end never offers an action the account cannot take.

There is no conversion. An account is one side of the marketplace or the other
for its whole life, so somebody who registers as the wrong one needs a new
account under a different address — which, with no password reset and no email
verification, is worth saying on the form. It is.

Google sign-in cannot ask, because the account is created from a token rather
than a form; it defaults to homeowner, the side that cannot claim. Enabling it
(issue #4) needs a type-selection step first.

**Licence data.** An operator-supplied CSLB file becomes `license_records` with
the source row preserved verbatim, and `contractors` — our own record, which an
import may refresh but never overwrite where a claimant has written.

**Location.** Every contractor is placed at its ZIP centroid on import, then
geocoded in the background. The published point is the centroid unless a
claimant has explicitly opted to publish their address. Search reads the same
point, so the radius filter cannot be used to recover a protected address.

**Search.** PostgreSQL full-text with accent folding, trigram matching on names,
PostGIS radius and viewport queries, trade and ZIP filters, keyset pagination.

**Claims and the badge.** A claim is opened by a user and decided by a
moderator. The verified badge is computed by one function from the linked
licence and the approved claim, never from a request, and the reason is stored
and shown.

**Messaging.** Direct messages to claimed contractors that opted in, with
gapless per-conversation sequencing, polling, blocking in both directions,
reporting, and per-account rate limits.

**Operations.** Health and readiness, structured JSON logs with request ids,
graceful shutdown, a schema gate that refuses to serve against a stale database,
bounded retention for every table that grows, and a backup script with a
companion that proves the backups restore.

## What is a placeholder, and what that means

| Area | State | What is needed |
|---|---|---|
| **CSLB column names** | The importer accepts several plausible spellings per field and **fails loudly listing what it saw** when a required one is missing. It has not been run against a real CSLB download in this environment. | Run `import-cslb --dry-run` against the real file. If it refuses, add the actual column name to `FIELD_ALIASES` in `crates/cm-domain/src/import.rs`. One line per column. |
| **CSLB download** | Manual. The portal serves files through an ASP.NET postback, not a stable URL. | Nothing, unless unattended refresh is wanted. Then a small fetcher that round-trips the viewstate — deliberately not written, because it would be a brittle dependency load-bearing on a cron nobody watches. |
| **ZIP centroids** | `deploy/data/zcta_la_county.csv` carries 25 LA County ZIPs, taken from the existing front end. | Load the full Census ZCTA gazetteer for complete county coverage. A contractor in a ZIP with no centroid is *unlocated* and absent from distance search — `cm-server prune --report-only` and the unlocated count make that visible. |
| **Password reset / email verification** | The `auth_tokens` table exists; no endpoint issues or consumes a token. | An SMTP path, then the two flows. Until then a user who forgets their password needs an operator. |
| **Apple sign-in** | Not implemented, by decision. | Direct OIDC, never through Firebase, if a native iOS app ships. |
| **Firebase against a real project** | Verification is tested against a locally generated key pair and in emulator mode. Google's live key document has not been fetched here. | Point `FIREBASE_PROJECT_ID` at the real project and sign in once. The failure mode is loud: verification fails closed. |
| **CI** | `.github/workflows/ci.yml` is written and has never run on GitHub. | Push and watch it. |
| **Load testing** | Measured by hand on this machine (below). No `k6` run against a full-county dataset. | Import the real file, then measure search p95. |
| **Stage 2** | Jobs, bids, reviews, payments, WebSockets, file uploads. Not built, and **no stub tables** — a table nobody validates would be wrong by the time it was needed. | Out of scope. |

## Measured behaviour on this machine

Release build, PostgreSQL 16 + PostGIS 3.4, 4 contractors, live Census geocoder.

| | |
|---|---|
| Resident memory at rest | 7.7 MB |
| After 2 000 directory reads | 9.4 MB |
| After 100 real logins (Argon2, concurrency 4) | 28.7 MB |
| After 500 real logins | 28.74 MB — **flat**, 36 KB drift over 400 further hashes |
| Peak database connections held | 2 |
| Graceful shutdown | exits 0, no error lines |

The step to ~29 MB is Argon2's arena retained by the allocator, bounded by
`CM_ARGON2_MAX_CONCURRENCY × 19 MiB`. It does not grow with load. Lower the
concurrency to trade login throughput for memory.

## Risks worth carrying into the first week

**Unclaimed listings are published. Decided 2026-08-24.** Tens of thousands of
businesses appear here without having signed up, many of them sole traders whose
CSLB address of record is their home. The decision is to publish the aggregated
directory.

That decision needed no code change. Centroid-only publication for anything
unclaimed is what the schema already enforces, and search reads the same
protected point. It was the other answer — claimed listings only — that would
have added a condition to the search predicate, and it is not being taken.

What remains open is **takedown**. Removal is an operator action today: there is
no self-service endpoint, no named owner and no target response time. Requests
to be delisted arrive regardless of the decision above, and right now they have
nowhere to land. Name an owner and write the procedure into `runbook.md` before
the first real import goes live.

**The badge is only as fresh as the last import.** Statuses change between
downloads. The detail endpoint returns `license_data_as_of` so a client can say
"as of" rather than implying it is live, and a nightly job re-derives every
badge — but the underlying data is a snapshot.

**One box, one database, no replica.** Recovery is a restore. `restore-verify.sh`
proves the backups work; run it on a timer and read its output.

**`CM_HASH_PEPPER` has no rotation path.** Rotating it invalidates outstanding
CSRF tokens and orphans IP digests. It does not log anyone out. There is no
dual-pepper transition — rotate in a quiet window, or build one first.

**The geocoder is a free public service.** The worker is rate-limited to two
requests a second and backs off on failure, but the Census Bureau owes us
nothing. A paid provider drops in behind the same trait.

## Things that will bite if forgotten

- Deploy order is **migrate, then restart**. `serve` refuses to start against a
  schema older than itself, so getting it backwards produces a clear failure
  rather than a subtle one.
- `CM_TRUST_PROXY_HEADERS=true` is required behind Caddy, or every request is
  attributed to one loopback address and per-IP limits become global. The header
  is ignored from any non-loopback peer regardless, so setting it is safe.
- The API must share a hostname with the front end. `__Host-` cookies are
  host-only; an API on a subdomain cannot read them.
- Enable the `cm-prune.timer`. Sessions and finished geocode jobs are the two
  tables that grow; nothing else deletes them.
