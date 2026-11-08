#!/bin/sh
# Back up OwnHealth into restic: a consistent backup directory (database dump, blobs, manifest with
# the state_hash of every profile), one restic snapshot of it, then retention.
#
#   ./deploy/restic/backup.sh
#   cron: 15 3 * * * cd /opt/ownhealth && ./deploy/restic/backup.sh >> backup.log 2>&1
#
# Retention: OWNHEALTH_RESTIC_KEEP (default 7 daily, 5 weekly, 12 monthly snapshots).
set -eu
. "$(dirname "$0")/common.sh"
RUN=${OWNHEALTH_RUN-docker compose exec -T app}
KEEP=${OWNHEALTH_RESTIC_KEEP:---keep-daily 7 --keep-weekly 5 --keep-monthly 12}

log "writing $STAGING"
$RUN rm -rf "$STAGING"
$RUN ownhealth backup "$STAGING" >/dev/null
log "restic backup"
$RUN restic backup --quiet --host ownhealth --tag ownhealth "$STAGING"
$RUN rm -rf "$STAGING"
log "restic forget $KEEP"
# shellcheck disable=SC2086 # KEEP is a list of flags
$RUN restic forget --quiet --host ownhealth --tag ownhealth $KEEP --prune
log "done"
