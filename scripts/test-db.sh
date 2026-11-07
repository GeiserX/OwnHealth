#!/usr/bin/env bash
# Start (or reuse) the shared test Postgres: container ownhealth-testpg on 127.0.0.1:55432.
# Idempotent. Tests create their own uniquely named database inside it and drop it after.
set -euo pipefail

NAME=ownhealth-testpg
IMAGE=postgres:17.11
PORT=55432

state=$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null || echo missing)
case "$state" in
  true) ;;
  false) docker start "$NAME" >/dev/null ;;
  *)
    docker run -d --name "$NAME" \
      -e POSTGRES_PASSWORD=postgres \
      -p 127.0.0.1:${PORT}:5432 \
      --tmpfs /var/lib/postgresql/data:rw \
      "$IMAGE" -c fsync=off -c synchronous_commit=off -c full_page_writes=off -c max_connections=300 >/dev/null
    ;;
esac

for _ in $(seq 1 60); do
  if docker exec "$NAME" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1; then
    echo "ownhealth-testpg ready on 127.0.0.1:${PORT}"
    exit 0
  fi
  sleep 0.5
done
echo "ownhealth-testpg did not become ready" >&2
exit 1
