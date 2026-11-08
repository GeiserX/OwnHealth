#!/bin/sh
# Restore a restic snapshot (default: the latest) into a fresh OwnHealth stack: a new database volume
# and a new data volume, with the app not started yet. Afterwards it checks every profile's state_hash
# against the manifest and exits 1 on any difference. `docker compose up -d` afterwards runs the admin
# service, which grants the app role its rows on the restored tables. Steps: deploy/README.md "Restore".
#
#   docker compose up -d db
#   ./deploy/restic/restore.sh
#   docker compose up -d
set -eu
. "$(dirname "$0")/common.sh"
RUN=${OWNHEALTH_RUN-docker compose run --rm -T admin}

# The dump carries the app role's grants, so the role must exist before pg_restore loads them.
# compose.yaml mounts app-role.sh into the admin container; elsewhere (OWNHEALTH_RUN="") it is skipped.
ROLE=${OWNHEALTH_APP_ROLE_SCRIPT:-/opt/ownhealth-db/app-role.sh}
$RUN sh -c "rm -rf '$SCRATCH' \
  && { [ ! -f '$ROLE' ] || sh '$ROLE'; } \
  && restic restore --quiet --host ownhealth --tag ownhealth '$SNAPSHOT:$STAGING' --target '$SCRATCH' >&2 \
  && ownhealth restore '$SCRATCH'; status=\$?; rm -rf '$SCRATCH'; exit \$status"
log "restored; start the app with: docker compose up -d"
