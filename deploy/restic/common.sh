# Shared settings for the restic scripts. Sourced, not run.
#
# Every command runs inside the app container, which ships restic and the PostgreSQL 17 client and
# reads RESTIC_REPOSITORY and RESTIC_PASSWORD_FILE from its environment (.env). Run the scripts from
# the folder that holds compose.yaml. OWNHEALTH_RUN replaces the `docker compose ...` prefix, for
# example OWNHEALTH_RUN="" to run everything on this machine.

# The staging directory sits on the data volume, so `ownhealth backup` hard-links the blobs into it
# instead of copying them. restic stores it under this path; restore.sh and restore-test.sh read it back.
STAGING=${OWNHEALTH_BACKUP_STAGING:-/data/backups/staging}
SCRATCH=${OWNHEALTH_RESTORE_DIR:-/data/backups/restore}
SNAPSHOT=${1:-latest}

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
