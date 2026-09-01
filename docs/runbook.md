# Runbook

Everything an operator needs that is not obvious from the code.

For what the system is made of and why — the stack, the crates, the schema
invariants, the pipelines — see [`architecture.md`](architecture.md). This file
is the procedure; that one is the map.

## First deploy, in order

```bash
# 1. Postgres, as a superuser, once per database.
createdb cm
psql -d cm -c 'CREATE EXTENSION postgis; CREATE EXTENSION pg_trgm; CREATE EXTENSION unaccent;'
```

Extensions need a privilege the service role does not have, and should not. The
migration that creates them is `IF NOT EXISTS`, so it is a no-op once they are
in place.

```bash
# 2. Two roles: one that owns the schema, one that runs the service.
psql -d cm <<'SQL'
CREATE ROLE cm_migrate LOGIN PASSWORD '...';
CREATE ROLE cm_app     LOGIN PASSWORD '...';
GRANT ALL ON SCHEMA public TO cm_migrate;
GRANT USAGE ON SCHEMA public TO cm_app;
ALTER DEFAULT PRIVILEGES FOR ROLE cm_migrate IN SCHEMA public
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO cm_app;
SQL
```

The runtime role has no DDL. A compromised application process cannot drop a
table.

```bash
# 3. Secrets.
install -d -m 0750 -o root -g cm /etc/cm-backend
umask 077
cat > /etc/cm-backend/env <<ENV
DATABASE_URL=postgres://cm_app:...@127.0.0.1/cm
CM_SITE_ORIGIN=https://app.example.com
CM_HASH_PEPPER=$(openssl rand -base64 48)
CM_ENV=production
CM_TRUST_PROXY_HEADERS=true
ENV
chown root:cm /etc/cm-backend/env && chmod 0640 /etc/cm-backend/env

# 4. Schema, then reference data, then the service.
cm-server check-config          # proves the file parses before anything restarts
DATABASE_URL=postgres://cm_migrate:...@127.0.0.1/cm cm-server migrate
cm-server seed-trades          # trades, their aliases, and re-derives contractor trades
cm-server load-regions --file deploy/data/zcta_ca.csv --source census_2020_gazetteer
# Then the place hierarchy — see "Loading places" below. Order matters:
# load-places reads the ZIP rows load-regions writes.
systemctl enable --now cm-server cm-geocode-worker cm-verification.timer
```

## Loading places

Cities, counties, and which ZIP codes belong to them. Four files, all published
by the Census, all plain text, all public domain.

They are **not vendored into the repository**. Reference data belongs to its
source: the Census republishes annually as cities file boundary changes through
the [Boundary and Annexation Survey](https://www.census.gov/programs-surveys/bas.html),
and a copy in git is a copy that silently goes stale. Download, load, delete.

```bash
cd /tmp && mkdir -p census && cd census

# Names, Census class (C* legally incorporated, U* census-designated) and county.
curl -sO https://www2.census.gov/geo/docs/reference/codes2020/national_place2020.txt

# Interior points — a coordinate guaranteed to fall inside the place, which a
# computed centroid is not for a concave city.
curl -sO https://www2.census.gov/geo/docs/maps-data/data/gazetteer/2024_Gazetteer/2024_Gaz_place_national.zip
curl -sO https://www2.census.gov/geo/docs/maps-data/data/gazetteer/2024_Gazetteer/2024_Gaz_counties_national.zip
unzip -oq '*.zip'

# ZIP-to-place, with the land area each pair shares. The Census already
# intersected the boundaries; this is a read, not a computation.
curl -sO https://www2.census.gov/geo/docs/maps-data/data/rel2020/zcta520/tab20_zcta520_place20_natl.txt

cm-server load-places \
  --places       national_place2020.txt \
  --place-points 2024_Gaz_place_national.txt \
  --counties     2024_Gaz_counties_national.txt \
  --zcta-places  tab20_zcta520_place20_natl.txt \
  --state CA
```

California loads 58 counties, 1,610 places and ~2,800 memberships in about
three seconds. Idempotent on `(kind, code)` — the Census GEOID — so re-running
after a new vintage updates in place.

Three numbers in the output are worth reading rather than skipping:

- **skipped below 0.5% of the ZIP** — pairs where two boundaries merely graze.
  Burbank and 90068 share 0.01 km², 0.0% of either, and without the threshold a
  Hollywood Hills ZIP would be "in Burbank". Raise `--min-share` if a real
  membership is ever dropped; the smallest genuine one in Los Angeles County is
  far above it.
- **ZIP name(s) superseded** — a curated ZIP label that only repeated the city
  it sits in. Twelve of the twenty-five hand-curated names are cities, not
  neighbourhoods (Burbank, Pasadena, Santa Monica…), and the city row is
  strictly better: it knows its county and holds all of its ZIPs. The thirteen
  real neighbourhoods — Silver Lake, Eagle Rock, Venice, Van Nuys — are
  untouched, because the Census has no record of them and they are what people
  actually type.
- **no gazetteer point** — a place named in one file and absent from the other.
  Skipped rather than given a guessed coordinate.

**Supply ranking.** `load-places` also counts listings per region, and
`recompute-verification` refreshes it nightly. That count is what orders place
suggestions, and it is what makes a statewide index safe on a Los Angeles
corpus: "san" offers San Pedro and San Gabriel ahead of San Francisco, because
those are the ones a homeowner can actually get a contractor from.

## The order that matters

`serve` **refuses to start** against a schema older than the binary. A deploy is
therefore always: build, `migrate`, restart. Getting it backwards produces a
service that will not start and a log line saying exactly why, rather than
handlers running against a schema they were not written for.

A database *ahead* of the binary is fine and keeps serving — every migration is
additive, so the middle of a rolling deploy is a valid state.

## Importing CSLB data

The file comes from the CSLB Public Data Portal's **Master List** (License
Master), which is free. Download it by hand: the portal serves its downloads
through an ASP.NET postback rather than a stable URL, so automating it would be
a brittle dependency load-bearing on a cron nobody watches.

```bash
cm-server import-cslb --file ./LicenseMaster.csv \
  --county "LOS ANGELES" --snapshot-date 2026-08-01 --dry-run   # look first
cm-server import-cslb --file ./LicenseMaster.csv \
  --county "LOS ANGELES" --snapshot-date 2026-08-01
```

- Re-running the **same bytes** is refused; re-running the same *content* is a
  no-op. Both are safe.
- The SHA-256 of the file and CSLB's own snapshot date are recorded, so any row
  can be traced to the download it came from.
- Malformed rows are counted and skipped; the run still succeeds. Check
  `rows_rejected` before trusting a run.
- **If the import fails naming missing columns**, CSLB has renamed something.
  The message lists the columns it saw; add the new name to `FIELD_ALIASES` in
  `crates/cm-domain/src/import.rs`. It fails rather than guessing on purpose.

After an import, badges are recomputed for every touched contractor
automatically. `cm-server recompute-verification` re-derives all of them and is
also on a nightly timer.

## Geocoding

The importer only queues; the worker resolves. A contractor is searchable at ZIP
precision from the moment it is imported and stays searchable if geocoding never
succeeds, so an outage degrades precision rather than removing listings.

```bash
cm-server geocode-worker --once                 # one pass, for a first look
systemctl status cm-geocode-worker
psql -d cm -c "SELECT status, count(*) FROM geocode_queue GROUP BY status;"
psql -d cm -c "SELECT count(*) FROM contractors WHERE public_point IS NULL;"
```

That last count is the one to watch: those contractors are absent from distance
search, and silently so.

## Backups

Installed as `cm-backup.timer`, nightly at 03:30 UTC — ahead of `cm-prune`
(04:15), so the night's backup captures the database as the day left it rather
than as housekeeping rewrote it.

```bash
systemctl list-timers cm-backup.timer
systemctl start cm-backup.service        # take one now
journalctl -u cm-backup.service -n 20    # why the last one failed
gcloud storage ls -l gs://cm-db-backups-6b1e669f/daily/
```

The unit runs `backup.sh` and then copies the result off the host, which is the
half the script deliberately leaves to the site. Both steps are `ExecStart`
lines, so a failed upload fails the unit rather than passing quietly with a
local-only backup.

**Configuration** lives in `/etc/cm-backup/env`, and the age identity beside it
in `/etc/cm-backup/identity.txt` (0400, owned by `postgres`). Deliberately not
`/etc/cm-backend/`: that directory is `root:cm` because it holds the
application's database password, and letting `postgres` read the backup key out
of it would hand the backup user the app's credentials as a side effect.

**Restores are verified, not assumed.** `restore-verify.sh` decrypts the newest
backup into a scratch database, checks the migration ledger and drops it again:

```bash
cd /tmp && sudo -u postgres bash -c 'set -a; . /etc/cm-backup/env; set +a; restore-verify.sh'
# restore verified from cm-20260831T010156Z.dump.age: migration 24, 30 tables, 0 dirty
```

Run it from a directory `postgres` can read — it uses `find`, which fails
trying to restore a working directory it was never allowed into.

Two retention windows, and they are not the same number: 30 days on the box
(`CM_BACKUP_KEEP_DAYS`), 90 in the bucket (a lifecycle rule). The bucket is the
one that matters; the local copy exists so `restore-verify.sh` has something
close at hand.

**The identity file is the whole backup.** It never leaves the box, which is the
point — the bucket is off-host and therefore untrusted, the box is not — but it
also means losing the box loses every backup taken with that key. Copy
`/etc/cm-backup/identity.txt` somewhere durable and outside GCP.

**`gsutil` must be the apt build, never the snap.** The snap cannot start under
`NoNewPrivileges=true`: snap-confine wants `cap_dac_override` and exits 1 before
transferring anything. `/snap/bin` comes first in the default PATH, so the unit
names `/usr/bin/gcloud` explicitly.

## When something is wrong

| Symptom | Look at |
|---|---|
| `cm-server` restart-loops | Its log. "refusing to serve" means run `migrate`. |
| `/readyz` is 503 | Its body names the cause: database unreachable, or migrations behind. |
| Everyone is logged out after a deploy | `CM_HASH_PEPPER` changed. It keys CSRF tokens; sessions survive, CSRF tokens do not. |
| One client is rate-limited unfairly | `CM_TRUST_PROXY_HEADERS` is false, so every request is attributed to Caddy's loopback address. Set it true — the service ignores the header from any non-loopback peer regardless. |
| Contractors missing from map search | `SELECT count(*) FROM contractors WHERE public_point IS NULL` and the geocode queue. |
| A badge looks wrong | `SELECT verified, verification_reason FROM contractors WHERE id = ...` — the reason is stored. Then `verification_checks` for that contractor. |

## Rotating the pepper

`CM_HASH_PEPPER` keys three things: IP digests, rate-limit bucket keys, and CSRF
token derivation. Rotating it:

- invalidates every outstanding CSRF token, so every open tab's next write fails
  once and succeeds after a reload;
- orphans existing IP digests, so old audit rows can no longer be correlated
  with new ones;
- resets every rate-limit bucket;
- does **not** log anybody out, and does **not** affect stored passwords.

There is no dual-pepper transition implemented. Rotate during a quiet window.

## What is deliberately not automated

- Downloading from the CSLB portal (see above).
- Copying backups off the host.
- Creating the first admin: `cm-server admin grant-role --email you@example.com
  --role admin`, from a shell. There is no endpoint that can create an admin.
