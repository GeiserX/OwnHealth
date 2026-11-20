#!/usr/bin/env bash
# Rebuild the upgrade fixtures in fixtures/upgrade/<version>/ from the released images on Docker Hub.
#
# For each version: a fresh Postgres 17, then the release's own `ownhealth serve` with OWNHEALTH_DEMO=1
# (it migrates and seeds the synthetic Rivera family at startup, as a demo install does), then the
# release's own `ownhealth backup`. The backup directory (manifest.json, db.dump, blobs/) is the fixture.
# server/crates/ownhealth-ops/tests/upgrade_fixtures.rs migrates each one with HEAD.
#
# 0.1.0 has no fixture of its own: 0.1.1 ships the same migrations, so its database covers 0.1.0.
# A fixture is written once per release. Rebuilding one draws new random src ids, so its golden.json
# must be recorded again; never rebuild a fixture to make a failing upgrade test pass.
# To record it: run `cargo test -p ownhealth-ops --test upgrade_fixtures --locked` in server/. The test
# for that version fails and prints the JSON HEAD gave; save it as fixtures/upgrade/<version>/golden.json.
#
# Usage: bash scripts/upgrade-fixtures.sh [version ...]    (default: 0.1.1 0.2.0 0.3.0 0.4.0)
set -euo pipefail

cd "$(dirname "$0")/.."
PG_IMAGE=postgres:17.11
versions=("$@")
[ ${#versions[@]} -gt 0 ] || versions=(0.1.1 0.2.0 0.3.0 0.4.0)

tag=ohfx-$$
cleanup() { docker rm -fv "$tag-app" "$tag-db" >/dev/null 2>&1 || true; docker network rm "$tag" >/dev/null 2>&1 || true; }
trap cleanup EXIT

for v in "${versions[@]}"; do
  echo "== $v"
  cleanup
  # Pull first: a stale local tag would give a database that release never wrote.
  docker pull -q "drumsergio/ownhealth:$v" >/dev/null
  docker network create "$tag" >/dev/null
  # A throwaway database on tmpfs; the password only exists inside this network for a minute.
  docker run -d --name "$tag-db" --network "$tag" --network-alias db \
    -e POSTGRES_PASSWORD=fixture-only --tmpfs /var/lib/postgresql/data:rw "$PG_IMAGE" >/dev/null
  for _ in $(seq 1 60); do
    docker exec "$tag-db" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 && break
    sleep 1
  done
  docker run -d --name "$tag-app" --network "$tag" \
    -e OWNHEALTH_DATABASE_URL=postgresql://postgres:fixture-only@db:5432/postgres \
    -e OWNHEALTH_DEMO=1 -e OWNHEALTH_RUN_JOBS=0 \
    "drumsergio/ownhealth:$v" ownhealth serve >/dev/null
  ok=
  for _ in $(seq 1 120); do
    if docker exec "$tag-app" ownhealth healthcheck >/dev/null 2>&1; then ok=1; break; fi
    sleep 1
  done
  if [ -z "$ok" ]; then docker logs "$tag-app" | tail -20 >&2; echo "$v did not start" >&2; exit 1; fi
  docker exec "$tag-app" ownhealth backup /data/fixture >/dev/null
  rm -rf "fixtures/upgrade/$v"
  mkdir -p fixtures/upgrade
  docker cp "$tag-app:/data/fixture" "fixtures/upgrade/$v" >/dev/null
  chmod -R u+rwX,go+rX "fixtures/upgrade/$v" # the backup is owner-only; the fixture is public synthetic data
  echo "fixtures/upgrade/$v: $(du -sh "fixtures/upgrade/$v" | cut -f1)"
done
