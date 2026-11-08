#!/bin/sh
# Restore a restic snapshot (default: the latest) into a fresh OwnHealth stack: a new database volume
# and a new data volume, with the app not started yet. Afterwards it checks every profile's state_hash
# against the manifest and exits 1 on any difference. Steps: deploy/README.md "Restore".
#
#   docker compose up -d db
#   ./deploy/restic/restore.sh
#   docker compose up -d
set -eu
. "$(dirname "$0")/common.sh"
RUN=${OWNHEALTH_RUN-docker compose run --rm -T app}

$RUN sh -c "rm -rf '$SCRATCH' \
  && restic restore --quiet --host ownhealth --tag ownhealth '$SNAPSHOT:$STAGING' --target '$SCRATCH' >&2 \
  && ownhealth restore '$SCRATCH'; status=\$?; rm -rf '$SCRATCH'; exit \$status"
log "restored; start the app with: docker compose up -d"
