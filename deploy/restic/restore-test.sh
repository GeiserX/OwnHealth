#!/bin/sh
# Prove a backup restores: take a restic snapshot (default: the latest), load it into a scratch
# database next to the live one, and compare row counts, blob hashes and every profile's state_hash
# with the manifest written at backup time. Prints the report; exits 1 on any difference.
#
#   ./deploy/restic/restore-test.sh            # the latest snapshot
#   ./deploy/restic/restore-test.sh 4be1a2c3   # one snapshot
#   cron: 45 4 * * 0 cd /opt/ownhealth && ./deploy/restic/restore-test.sh >> backup.log 2>&1
#
# The live database and blob store are only read. A backup that was never restored counts as missing.
set -eu
. "$(dirname "$0")/common.sh"
RUN=${OWNHEALTH_RUN-docker compose run --rm -T admin}

$RUN rm -rf "$SCRATCH"
log "restic restore $SNAPSHOT"
$RUN restic restore --quiet --host ownhealth --tag ownhealth "$SNAPSHOT:$STAGING" --target "$SCRATCH" >&2
status=0
$RUN ownhealth verify-restore "$SCRATCH" || status=$?
$RUN rm -rf "$SCRATCH"
if [ "$status" -eq 0 ]; then log "restore test passed"; else log "restore test FAILED"; fi
exit "$status"
