#!/usr/bin/env bash
# The server the browser checks run against (Playwright's webServer starts it, then stops it with
# SIGTERM): the debug binary on a fresh database in the shared test Postgres (scripts/test-db.sh),
# seeded with the synthetic demo household, serving the UI build from server/static. The data folder,
# and the database when this run created it, are dropped on exit. OWNHEALTH_E2E_DB names the database
# (the tests seed rows into it); connector requests may reach a fake vendor on loopback
# (OWNHEALTH_DEV_CONNECTOR_LOOPBACK).
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${OWNHEALTH_E2E_BIN:-$root/server/target/debug/ownhealth}
port=${OWNHEALTH_E2E_PORT:-8190}
admin=postgresql://postgres:postgres@127.0.0.1:55432/postgres # ownhealth-testpg, where psql_admin runs
db="${OWNHEALTH_E2E_DB:-oh_e2e_$(openssl rand -hex 6)}"
data=$(mktemp -d)
pid=""
created="" # set once this run made the database: a name that already exists is never dropped

[ -x "$bin" ] || { echo "no server binary at $bin: cargo build -p ownhealth (or set OWNHEALTH_E2E_BIN)" >&2; exit 1; }
[ -f "$root/server/static/index.html" ] || { echo "no UI build in server/static: pnpm build" >&2; exit 1; }

psql_admin() { docker exec ownhealth-testpg psql -U postgres -qAtc "$1" >/dev/null; }

cleanup() {
  if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  if [ -n "$created" ]; then psql_admin "drop database if exists \"$db\" with (force)" || true; fi
  rm -rf "$data"
}
trap cleanup EXIT
trap 'exit 143' TERM INT

bash "$root/scripts/test-db.sh" >/dev/null
psql_admin "create database \"$db\""
created=1

export OWNHEALTH_DATABASE_URL="${admin%/*}/$db"
export OWNHEALTH_DATA_DIR="$data"
export OWNHEALTH_STATIC_DIR="$root/server/static"
export OWNHEALTH_DEMO=1 OWNHEALTH_DEV_AUTH=1
export OWNHEALTH_HOST=127.0.0.1 OWNHEALTH_PORT="$port"
export OWNHEALTH_BASE_URL="http://127.0.0.1:$port"
export OWNHEALTH_SECRET_KEY="$(openssl rand -hex 48)"
export OWNHEALTH_SECRETS_KEY="$(openssl rand -base64 32)" # seals the connectors' refresh tokens
export OWNHEALTH_DEV_CONNECTOR_LOOPBACK=1 # debug build with dev auth only: the fake vendor on loopback
export RUST_LOG="${RUST_LOG:-warn}" # the access log stays out of the test output; errors still show

"$bin" serve &
pid=$!
wait "$pid"
