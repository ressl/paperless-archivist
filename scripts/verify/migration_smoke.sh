#!/usr/bin/env bash
set -euo pipefail

IMAGE="${POSTGRES_IMAGE:-postgres:18}"
NAME="${POSTGRES_CONTAINER_NAME:-paperless-archivist-migration-smoke-$$}"
PASSWORD="${POSTGRES_PASSWORD:-archivist}"
DATABASE="${POSTGRES_DB:-archivist}"
USER="${POSTGRES_USER:-archivist}"

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run --pull=missing -d \
  --name "$NAME" \
  -p 127.0.0.1::5432 \
  -e POSTGRES_PASSWORD="$PASSWORD" \
  -e POSTGRES_USER="$USER" \
  -e POSTGRES_DB="$DATABASE" \
  "$IMAGE" >/dev/null

until docker exec "$NAME" pg_isready -U "$USER" -d "$DATABASE" >/dev/null 2>&1; do
  sleep 1
done

PORT="$(docker port "$NAME" 5432/tcp | sed 's/.*://')"
export DATABASE_URL="postgres://${USER}:${PASSWORD}@127.0.0.1:${PORT}/${DATABASE}"
export ARCHIVIST_MIGRATIONS_DIR="${PWD}/migrations"

cargo test -p archivist-db --test migration_smoke -- --ignored --nocapture

echo "migration smoke ok: all migrations applied on ${IMAGE}"
