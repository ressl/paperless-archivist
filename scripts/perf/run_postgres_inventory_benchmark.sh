#!/usr/bin/env bash
set -euo pipefail

IMAGE="${POSTGRES_IMAGE:-postgres:18}"
NAME="${POSTGRES_CONTAINER_NAME:-paperless-archivist-perf-$$}"
PASSWORD="${POSTGRES_PASSWORD:-archivist}"
DATABASE="${POSTGRES_DB:-archivist}"
USER="${POSTGRES_USER:-archivist}"
SIZES="${BENCH_SIZES:-10000 50000 100000}"
REPORT_DIR="${REPORT_DIR:-target/perf}"
REPORT_FILE="${REPORT_FILE:-${REPORT_DIR}/postgres-inventory-benchmark.txt}"

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

mkdir -p "$REPORT_DIR"
: > "$REPORT_FILE"

docker run --pull=missing -d \
  --name "$NAME" \
  -e POSTGRES_PASSWORD="$PASSWORD" \
  -e POSTGRES_USER="$USER" \
  -e POSTGRES_DB="$DATABASE" \
  "$IMAGE" >/dev/null

until docker exec "$NAME" pg_isready -U "$USER" -d "$DATABASE" >/dev/null 2>&1; do
  sleep 1
done

until docker exec -e PGPASSWORD="$PASSWORD" "$NAME" \
  psql -v ON_ERROR_STOP=1 -U "$USER" -d "$DATABASE" -c 'select 1' >/dev/null 2>&1; do
  sleep 1
done

{
  echo "Paperless Archivist PostgreSQL inventory benchmark"
  echo "Image: ${IMAGE}"
  echo "Sizes: ${SIZES}"
  echo "Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
} | tee -a "$REPORT_FILE"

for migration in migrations/*.sql; do
  echo "Applying ${migration}" | tee -a "$REPORT_FILE"
  docker exec -e PGPASSWORD="$PASSWORD" -i "$NAME" \
    psql -v ON_ERROR_STOP=1 -U "$USER" -d "$DATABASE" < "$migration" >>"$REPORT_FILE"
done

for size in $SIZES; do
  {
    echo
    echo "================================================================"
    echo "Benchmark size: ${size} documents"
    echo "================================================================"
  } | tee -a "$REPORT_FILE"
  docker exec -e PGPASSWORD="$PASSWORD" -i "$NAME" \
    psql -v ON_ERROR_STOP=1 -U "$USER" -d "$DATABASE" \
      -v doc_count="$size" \
      -f /dev/stdin < scripts/perf/postgres_inventory_benchmark.sql | tee -a "$REPORT_FILE"
done

echo "Benchmark report written to ${REPORT_FILE}"
