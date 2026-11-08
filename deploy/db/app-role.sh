#!/bin/sh
# Create or update the app's Postgres role and its grants (app-role.sql). Runs in the `admin` service of
# compose.yaml, connected as the database owner, after `ownhealth migrate` and around a restore.
#
#   OWNHEALTH_DATABASE_URL                the owner's URL, without the password
#   OWNHEALTH_DB_PASSWORD[_FILE]          the owner's password
#   OWNHEALTH_APP_DB_PASSWORD[_FILE]      the app role's password
#   OWNHEALTH_APP_DB_ROLE                 the app role's name (default ownhealth_app)
set -eu
if [ -n "${OWNHEALTH_DB_PASSWORD_FILE:-}" ]; then PGPASSWORD=$(cat "$OWNHEALTH_DB_PASSWORD_FILE"); else PGPASSWORD=${OWNHEALTH_DB_PASSWORD:-}; fi
if [ -n "${OWNHEALTH_APP_DB_PASSWORD_FILE:-}" ]; then
  OWNHEALTH_APP_ROLE_PASSWORD=$(cat "$OWNHEALTH_APP_DB_PASSWORD_FILE")
else
  OWNHEALTH_APP_ROLE_PASSWORD=${OWNHEALTH_APP_DB_PASSWORD:-}
fi
if [ -z "$OWNHEALTH_APP_ROLE_PASSWORD" ]; then
  echo "error: set OWNHEALTH_APP_DB_PASSWORD (or OWNHEALTH_APP_DB_PASSWORD_FILE) for the app's database role" >&2
  exit 1
fi
export PGPASSWORD OWNHEALTH_APP_ROLE_PASSWORD
exec psql "$OWNHEALTH_DATABASE_URL" -X -q -v ON_ERROR_STOP=1 \
  -v app_role="${OWNHEALTH_APP_DB_ROLE:-ownhealth_app}" -f "$(dirname "$0")/app-role.sql"
