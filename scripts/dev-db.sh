#!/usr/bin/env bash
# Start (or reuse) a local PostgreSQL 17 for development and tests.
# Exports nothing; prints the URL to use as H5I_APP_TEST_DATABASE_URL / DATABASE_URL.
# The one place for the test server's settings: CI starts its PostgreSQL with
# this script too. max_connections=200: SERIALIZABLE's conflict pool
# (RWConflictPool) is sized from it, and the concurrency suite overflows the
# default 100's (SQLSTATE 53200).
set -euo pipefail
NAME=${H5I_PAY_PG_NAME:-h5i-pay-pg}
PORT=${H5I_PAY_PG_PORT:-55432}
if ! docker inspect "$NAME" >/dev/null 2>&1; then
  docker run -d --name "$NAME" -e POSTGRES_USER=h5i -e POSTGRES_PASSWORD=h5i -e POSTGRES_DB=h5i \
    -p "127.0.0.1:${PORT}:5432" postgres:17 -c max_connections=200 >/dev/null
elif [ "$(docker inspect -f '{{.State.Running}}' "$NAME")" != "true" ]; then
  docker start "$NAME" >/dev/null
fi
for _ in $(seq 1 60); do
  if docker exec "$NAME" pg_isready -U h5i >/dev/null 2>&1; then
    echo "postgres://h5i:h5i@127.0.0.1:${PORT}/h5i"
    exit 0
  fi
  sleep 1
done
echo "PostgreSQL did not become ready" >&2
exit 1
