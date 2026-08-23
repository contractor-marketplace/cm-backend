#!/usr/bin/env bash
# Weekly proof that the backups restore.
#
# A backup that has never been restored is a hope, not a backup. This restores
# the newest one into a scratch database, runs the migration ledger check, and
# throws the scratch database away. It is meant to run on a timer and to shout
# when it fails.
set -euo pipefail

: "${CM_BACKUP_DIR:=/var/backups/cm}"
: "${CM_BACKUP_IDENTITY:?set CM_BACKUP_IDENTITY to the age identity file}"
: "${PGHOST:=127.0.0.1}"
: "${PGUSER:=postgres}"

newest="$(find "$CM_BACKUP_DIR" -name 'cm-*.dump.age' -printf '%T@ %p\n' | sort -rn | head -1 | cut -d' ' -f2-)"
[ -n "$newest" ] || { echo "no backup found in $CM_BACKUP_DIR"; exit 1; }

scratch="cm_restore_check_$$"
work="$(mktemp -d)"
trap 'rm -rf "$work"; dropdb --if-exists "$scratch" >/dev/null 2>&1 || true' EXIT

age --decrypt --identity "$CM_BACKUP_IDENTITY" --output "$work/restore.dump" "$newest"

createdb "$scratch"
psql -d "$scratch" -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION IF NOT EXISTS postgis;' >/dev/null
pg_restore --dbname="$scratch" --no-owner --no-privileges "$work/restore.dump"

# The schema is only as restored as its migration ledger says it is.
applied="$(psql -d "$scratch" -tAc 'SELECT max(version) FROM _sqlx_migrations WHERE success')"
dirty="$(psql -d "$scratch" -tAc 'SELECT count(*) FROM _sqlx_migrations WHERE NOT success')"
tables="$(psql -d "$scratch" -tAc "SELECT count(*) FROM pg_class WHERE relkind='r' AND relnamespace='public'::regnamespace")"

[ "$dirty" = "0" ] || { echo "restored schema has $dirty incomplete migration(s)"; exit 1; }
[ "$tables" -gt 10 ] || { echo "restored schema has only $tables tables; that is not a full restore"; exit 1; }

echo "restore verified from $(basename "$newest"): migration $applied, $tables tables, 0 dirty"
