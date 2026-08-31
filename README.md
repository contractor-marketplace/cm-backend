# cm-backend

The contractor marketplace v1 backend: Rust, axum, sqlx, PostgreSQL 16 with
PostGIS, deployed on a single self-hosted VPS.

**Current state: v1 feature-complete.** Password, Google and Facebook
authentication; CSLB licence import; background geocoding; PostGIS search and
map; contractor claims with an auditable verified badge and a moderation queue;
a structured job board with photos; a claimant-owned profile; and ~11,000
published Google reviews.

| Document | Read it for |
|---|---|
| **`docs/architecture.md`** | **The whole stack and everything that has been built — start here** |
| `docs/search.md` | Search and queries: retrieval, ranking, pagination, facets, and how they are measured |
| `docs/runbook.md` | Deploying, importing, backups, what to do when something breaks |
| `docs/handover.md` | What is and is not covered |
| This file | Running it locally |

## Layout

| Crate | Responsibility |
|---|---|
| `cm-core` | Configuration, the error taxonomy, UUIDv7 identifiers, telemetry. No I/O. |
| `cm-db` | Connection pool, migrations, repositories. The only crate that contains SQL. |
| `cm-auth` | Argon2id hashing, sessions, cookies, CSRF, rate limits, Firebase token verification. |
| `cm-domain` | Business rules: import, geocoding, location privacy, verification, claims, messaging, search input. |
| `cm-api` | axum router, handlers, DTOs. |
| `cm-server` | The binary and every operator command. |

## Requirements

- Rust 1.97.1, pinned in `rust-toolchain.toml`. Install it explicitly:

  ```bash
  rustup toolchain install 1.97.1 --profile minimal --component rustfmt --component clippy
  ```

  **`~/.cargo/bin` must be on your `PATH`.** A rustup installation does not
  always put it there — on a machine where Rust is installed but bare `cargo`
  reports "command not found", this is why:

  ```bash
  export PATH="$HOME/.cargo/bin:$PATH"    # add to your shell profile to persist
  ```

- PostgreSQL 16 with PostGIS 3, `pg_trgm` and `unaccent`

## Local database

With Docker:

```bash
docker run --rm -d --name cm-pg -p 5432:5432 \
  -e POSTGRES_USER=cmdev -e POSTGRES_PASSWORD=cmdev -e POSTGRES_DB=cm_dev \
  postgis/postgis:16-3.4
```

Without Docker, any PostgreSQL 16 with the `postgis`, `pg_trgm` and `unaccent`
extensions available will do. The migration that installs them needs a role with
the privilege to `CREATE EXTENSION`; in production that is the migration role,
which is separate from the role the service runs as.

## Running

```bash
cp .env.example .env          # then edit DATABASE_URL
set -a && . ./.env && set +a

cargo run -p cm-server -- check-config   # validate the environment, print it redacted
cargo run -p cm-server -- migrate        # apply outstanding migrations
cargo run -p cm-server -- serve          # serve the API
```

```bash
curl -s localhost:8080/healthz   # liveness; never touches the database
curl -s localhost:8080/readyz    # readiness; 503 until the schema matches
curl -s localhost:8080/version   # build and expected schema version
```

## Authentication

| Method and path | Session | CSRF |
|---|---|---|
| `POST /v1/auth/register` | — | — |
| `POST /v1/auth/login` | — | — |
| `POST /v1/auth/google` | — | — |
| `GET /v1/me` | required | — (safe method) |
| `POST /v1/auth/logout` | required | required |
| `POST /v1/auth/logout-all` | required | required |
| `POST /v1/auth/password` | required | required |
| `POST /v1/auth/link/google` | required | required |

### Google sign-in

Firebase runs the Google dance in the browser and hands back an ID token; this
service verifies it and forgets it. Only Google's public keys are needed, so no
service-account credential is handled anywhere and there is no Admin SDK.
Firebase is not a directory, not a session store, and not consulted again.

An account is resolved by `(provider, subject)` — Google's own account id, taken
from the verified token — and **never by email address**. Someone who registered
with a password and then signs in with Google gets a separate account; joining
them requires being signed in to the first one and calling
`POST /v1/auth/link/google`. That is the deliberate cost of not letting anyone
who can obtain a Google account bearing an address obtain the account behind it.

Set `FIREBASE_PROJECT_ID` to enable it. Without it, the endpoint answers "not
configured" rather than half-working.

Passwords are hashed with Argon2id at 19 MiB, two passes, one lane. Sessions are
32 bytes of OS randomness, stored only as their SHA-256 digest, delivered in a
`__Host-cm_session` cookie (`Secure`, `HttpOnly`, `SameSite=Lax`, `Path=/`, no
`Domain`).

**The `__Host-` prefix has a deployment consequence.** Such a cookie is
host-only: it cannot be shared with a different host, so the API must be served
from the same origin as the front end — `https://app.example.com/api/*` proxied
to this service, not `https://api.example.com`.

State-changing authenticated requests must send `X-CM-CSRF` matching the
`__Host-cm_csrf` cookie. The expected value is *derived* from the session with a
keyed hash rather than merely compared against the cookie, so writing a cookie
on the site's host — the usual way a double-submit scheme fails — does not
produce a token that matches. `Origin`, when present, must match
`CM_SITE_ORIGIN`.

Limits: registration 10/hour per address, login 20/15 minutes per address,
password change 5/hour per account, and an account lock for 15 minutes after 8
consecutive failures. Rate-limit bucket keys are stored as peppered digests, and
elapsed windows are swept in bounded batches by a background task.

### Identifying the client

`X-Forwarded-For` is believed only when `CM_TRUST_PROXY_HEADERS` is set **and**
the immediate socket peer is loopback. The flag alone would trust the header
from whoever connected, so anything able to reach the port could choose its own
rate-limit bucket and its own audit trail. The deployment is exactly one Caddy
hop on `127.0.0.1`, and that boundary is enforced here rather than left to the
firewall to enforce on this code's behalf. When the header is believed, the
**last** entry is taken — the one the trusted proxy appended, and the only one
a client cannot forge.

### Hashing and the connection pool

No pooled database connection is ever held across an Argon2 hash. Each hash
holds ~19 MiB for tens of milliseconds and queues behind a semaphore, so a
connection held across one would let a burst of logins check out the whole pool
and starve every other query, readiness checks included.

Authentication therefore runs in three phases: read the credential and release
the connection; hash while holding nothing; then re-read under a row lock and
revalidate before acting. The revalidation is not a formality — between the read
and the act the password can be changed, the account suspended, or the
credential locked. A login that verified a password which has since been
replaced is refused, and two password changes that both verified the same old
password result in exactly one success, the loser getting a 409.

### Local development over http

`__Host-` cookies require `Secure`. Chrome and Firefox both accept `Secure`
cookies on `http://localhost`, so local development works; Safari does not, and
there is deliberately no configuration switch to weaken the cookie, because a
switch that downgrades authentication is a switch that eventually gets set in
production. Develop against Chrome or Firefox, or put a local TLS proxy in
front.

## The public API

| Method and path | Session | Notes |
|---|---|---|
| `GET /v1/contractors` | — | Keyset-paginated. Text, trade, ZIP, radius, bbox, verified. |
| `GET /v1/contractors/map` | — | Same predicate, capped at 500 points, `truncated` flag. |
| `GET /v1/contractors/{id\|slug}` | — | Detail plus the evidence behind the badge. |
| `GET /v1/suggest` | — | Typeahead: trades, places and businesses. Rate limited per address. |
| `GET /v1/jobs/map` | — | Open jobs as pins. Same predicate as the board, capped at 500. |
| `GET /v1/trades`, `GET /v1/regions` | — | Filter vocabularies. |
| `PATCH /v1/contractors/{id}` | claimant | Bio, phone, website, DM opt-in, address visibility. |
| `POST /v1/contractors/{id}/claims` | required | Open a claim. |
| `GET /v1/me/claims`, `POST /v1/me/claims/{id}/withdraw` | required | Own claims. |
| `GET\|PUT /v1/me/homeowner-profile` | required | Optional profile. |
| `GET\|POST /v1/conversations` | required | DM list and creation. |
| `GET\|POST /v1/conversations/{id}/messages` | required | Poll and send. |
| `POST /v1/conversations/{id}/read` | required | Advance the read cursor. |
| `GET /v1/blocks`, `PUT\|DELETE /v1/blocks/{user_id}` | required | Blocking. |
| `POST /v1/reports` | required | Report a message or conversation. |
| `GET /v1/admin/claims`, `POST /v1/admin/claims/{id}/decide` | moderator | Claim queue. |
| `GET /v1/admin/reports` | moderator | Report queue. |

### Location privacy

Every read path publishes `public_point` and never `precise_point`. For an
unclaimed listing — or any listing whose owner has not opted in — that is the
ZIP-code centroid, because CSLB sole-owner records frequently carry a home
address.

Search reads the same point. That is the part that is easy to get wrong: if
distance search ran against the precise point while the map published a
centroid, the radius filter could be binary-searched to recover the address the
centroid was protecting. There is a test that performs exactly that attack and
asserts it fails.

### The verified badge

Computed by one function, from the linked CSLB licence and an approved claim.
Both halves are required: a licence alone describes a business, not the person
holding the account; a claim alone is an assertion nobody checked. The reason is
stored alongside the badge and returned by the detail endpoint, so "why is this
verified" is answerable months later.

`PATCH /v1/contractors/{id}` **rejects** a request body mentioning `verified`
rather than ignoring it — silently ignoring it would teach a client it worked.

## Operator commands

```bash
cm-server admin show-roles  --email person@example.com
cm-server admin grant-role  --email person@example.com --role admin
cm-server admin revoke-role --email person@example.com --role admin

cm-server import-cslb --file ./LicenseMaster.csv --county "LOS ANGELES" [--dry-run]
cm-server load-regions --file deploy/data/zcta_ca.csv --source census_2020_gazetteer
cm-server seed-trades
cm-server recompute-verification
cm-server geocode-worker [--once]
```

Roles are granted from a shell, never over HTTP. The first admin has to come
from somewhere, and an endpoint that can create admins is an endpoint worth
attacking; shell access to the box is a stronger prerequisite than any check
that endpoint could make. Every grant and revoke writes an `audit_log` row
recording that it came from the CLI.

`serve` deliberately does not migrate, and **refuses to start if the database
is behind this binary**. A deploy runs `migrate` once and then restarts the
unit, so two instances can never race to change the schema, and a process that
did start is a process whose schema is usable. Nothing guarantees that the
reverse proxy consults `/readyz`, so an unusable schema has to stop the process
rather than only change what it reports.

A database *ahead* of the binary is allowed and keeps serving: migrations are
additive, so the middle of a rolling deploy is a valid state.

## Tests

```bash
cargo test --workspace        # needs DATABASE_URL; creates its own test databases
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Integration tests run against a real PostgreSQL with PostGIS. Nothing in the
suite mocks the database: partial unique indexes, CHECK constraints and PostGIS
types are precisely what a mock cannot reproduce, and they are what carries the
guarantees.

`cargo build` does **not** need a database. Queries are runtime-checked rather
than macro-checked, so there is no `.sqlx` metadata directory; committing
generated metadata is deferred until the schema and its queries have been
reviewed.

## Migrations

Forward-only, embedded in the binary at compile time, checksum-verified by sqlx.
An applied migration is never edited — corrections ship as a new file. Every
migration must also be compatible with the previously deployed binary, so that a
rolling deploy (migrate, then restart) is safe in both orders.

| Version | File | Contents |
|---|---|---|
| 0001 | `0001_extensions.sql` | `postgis`, `pg_trgm`, `unaccent`, and the `english_unaccent` text-search configuration |
| 0002 | `0002_reference_data.sql` | `regions`, `trades` |
| 0003 | `0003_accounts.sql` | `users`, `user_roles` |
| 0004 | `0004_password_credentials.sql` | `password_credentials` |
| 0005 | `0005_sessions.sql` | `sessions`, `auth_tokens` |
| 0006 | `0006_audit_log.sql` | `audit_log` |
| 0007 | `0007_rate_limit_counters.sql` | `rate_limit_counters` |
| 0008 | `0008_oauth_identities.sql` | `oauth_identities` |
| 0009 | `0009_license_records.sql` | `license_import_runs`, `license_records`, `license_record_versions` |
| 0010 | `0010_contractors.sql` | `contractors`, `contractor_trades`, `contractor_service_areas` |
| 0011 | `0011_geocode_queue.sql` | `geocode_queue` |
| 0012 | `0012_claims.sql` | `contractor_claims`, `verification_checks` |
| 0013 | `0013_homeowner_profiles.sql` | `homeowner_profiles` |
| 0014 | `0014_messaging.sql` | `conversations`, `conversation_participants`, `messages` |
| 0015 | `0015_safety.sql` | `user_blocks`, `message_reports` |

`auth_tokens` is created but unused: password reset and email verification need
a mail path that does not exist yet, so no endpoint issues or consumes a token.
The table lands with the rest of the auth schema so those flows are a code
change rather than a migration when the mail path is approved.
