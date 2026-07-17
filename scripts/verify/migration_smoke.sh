#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="${POSTGRES_IMAGE:-postgres:18}"
NAME="${POSTGRES_CONTAINER_NAME:-paperless-archivist-migration-smoke-$$}"
PASSWORD="${POSTGRES_PASSWORD:-archivist}"
DATABASE="${POSTGRES_DB:-archivist}"
USER="${POSTGRES_USER:-archivist}"
STARTED_CONTAINER=false
RESULTS_FILE=""

cleanup() {
  if [[ -n "$RESULTS_FILE" ]]; then
    rm -f "$RESULTS_FILE"
  fi
  if [[ "$STARTED_CONTAINER" == true ]]; then
    docker rm -f "$NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ -z "${DATABASE_URL:-}" ]]; then
  command -v docker >/dev/null 2>&1 || {
    echo "DATABASE_URL is unset and Docker is unavailable" >&2
    exit 1
  }

  docker run --pull=missing -d \
    --name "$NAME" \
    -p 127.0.0.1::5432 \
    -e POSTGRES_PASSWORD="$PASSWORD" \
    -e POSTGRES_USER="$USER" \
    -e POSTGRES_DB="$DATABASE" \
    "$IMAGE" >/dev/null
  STARTED_CONTAINER=true

  for _ in $(seq 1 60); do
    if docker exec "$NAME" pg_isready -U "$USER" -d "$DATABASE" >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done

  if ! docker exec "$NAME" pg_isready -U "$USER" -d "$DATABASE" >/dev/null 2>&1; then
    echo "PostgreSQL test container did not become ready" >&2
    exit 1
  fi

  PORT="$(docker port "$NAME" 5432/tcp | sed 's/.*://')"
  export DATABASE_URL="postgres://${USER}:${PASSWORD}@127.0.0.1:${PORT}/${DATABASE}"
  DATABASE_MODE="${IMAGE} Docker container"
else
  DATABASE_MODE="external PostgreSQL service"
fi

export ARCHIVIST_MIGRATIONS_DIR="${ARCHIVIST_MIGRATIONS_DIR:-${ROOT_DIR}/migrations}"

EXPECTED_TESTS="$({
  grep -R -E '^[[:space:]]*#\[ignore' "${ROOT_DIR}/crates/archivist-db/tests" --include='*.rs' || true
} | wc -l | tr -d '[:space:]')"

if [[ "$EXPECTED_TESTS" -eq 0 ]]; then
  echo "No ignored database integration tests found" >&2
  exit 1
fi

RESULTS_FILE="$(mktemp)"
set +e
(
  cd "$ROOT_DIR"
  cargo test -p archivist-db --tests --locked -- --ignored --nocapture --test-threads=1
) 2>&1 | tee "$RESULTS_FILE"
CARGO_STATUS=${PIPESTATUS[0]}
set -e

EXECUTED_TESTS="$(sed -n 's/.* \([0-9][0-9]*\) passed;.*/\1/p' "$RESULTS_FILE" | awk '{ total += $1 } END { print total + 0 }')"
SKIPPED_TESTS="$(sed -n 's/.* \([0-9][0-9]*\) ignored;.*/\1/p' "$RESULTS_FILE" | awk '{ total += $1 } END { print total + 0 }')"

echo "database integration summary: executed=${EXECUTED_TESTS} skipped=${SKIPPED_TESTS} expected=${EXPECTED_TESTS}"

if [[ "$CARGO_STATUS" -ne 0 ]]; then
  exit "$CARGO_STATUS"
fi

if [[ "$EXECUTED_TESTS" -ne "$EXPECTED_TESTS" || "$SKIPPED_TESTS" -ne 0 ]]; then
  echo "Database integration test count mismatch" >&2
  exit 1
fi

echo "database integration ok: migrations and all tests passed on ${DATABASE_MODE}"
