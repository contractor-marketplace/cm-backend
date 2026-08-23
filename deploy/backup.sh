#!/usr/bin/env bash
# Nightly logical backup.
#
# Custom format, so a restore can be selective. Encrypted before it leaves the
# box, because the destination is off-box by design: a backup on the same disk
# as the database is not a backup.
set -euo pipefail

: "${CM_BACKUP_DIR:=/var/backups/cm}"
: "${CM_BACKUP_KEEP_DAYS:=30}"
: "${CM_BACKUP_RECIPIENT:?set CM_BACKUP_RECIPIENT to the age/gpg recipient}"
: "${DATABASE_URL:?set DATABASE_URL}"

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$CM_BACKUP_DIR"
dump="$CM_BACKUP_DIR/cm-$stamp.dump"

pg_dump --format=custom --no-owner --no-privileges --file="$dump" "$DATABASE_URL"

# Verify the dump is readable before anything is deleted on its strength.
pg_restore --list "$dump" >/dev/null

age --encrypt --recipient "$CM_BACKUP_RECIPIENT" --output "$dump.age" "$dump"
rm -f "$dump"

# Retention runs last, and only after a good dump exists.
find "$CM_BACKUP_DIR" -name 'cm-*.dump.age' -mtime "+$CM_BACKUP_KEEP_DAYS" -delete

echo "backup complete: $dump.age ($(du -h "$dump.age" | cut -f1))"
echo "REMINDER: copy it off this host. A backup on the same disk is not a backup."
