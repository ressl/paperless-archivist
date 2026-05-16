#!/usr/bin/env bash
# Drive a full end-to-end pipeline run against the dev-local stack.
#
# Assumes the stack is up and ./scripts/dev-local-bootstrap.sh + the seed have
# already populated local Paperless with sample documents.
#
# Phase 1 (manual_review):
#   - POST /api/paperless/sync-metadata     # pull docs into the archivist
#   - POST /api/batches/full                # queue OCR + Metadata for everything
#   - poll /api/dashboard until queued+running == 0
#   - GET /api/reviews?status=pending&limit=200
#   - POST /api/reviews/{id}/approve for each pending item
#
# Phase 2 (full_auto):
#   - PUT /api/workflow/mode {"mode":"full_auto"}
#   - re-upload one document into local Paperless via the seed script (NEW title)
#   - POST /api/paperless/sync-metadata
#   - POST /api/batches/full
#   - poll dashboard, verify the new doc landed in `applied` without manual review
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_FILE="${SCRIPT_DIR}/.dev-local-state.json"
COOKIE_JAR="${SCRIPT_DIR}/.dev-local-cookies.txt"

if [[ ! -f "${STATE_FILE}" ]]; then
  echo "missing ${STATE_FILE}; run ./scripts/dev-local-bootstrap.sh first" >&2
  exit 2
fi

ARCHIVIST_URL="$(jq -r .archivist_url "${STATE_FILE}")"
CSRF_TOKEN="$(jq -r .csrf_token "${STATE_FILE}")"

log() { printf '[e2e] %s\n' "$*" >&2; }

# Refresh session/CSRF — the cookie jar may be stale on second invocations.
refresh_session() {
  log "refreshing archivist session ..."
  local env_file="${SCRIPT_DIR}/../deploy/compose/.env.dev-local"
  if [[ -f "${env_file}" ]]; then
    set -a; source "${env_file}"; set +a
  fi
  local response
  response="$(curl -fsS -c "${COOKIE_JAR}" -X POST \
    -H 'Content-Type: application/json' \
    -d "$(jq -n --arg u "${ARCHIVIST_ADMIN_USERNAME}" --arg p "${ARCHIVIST_ADMIN_PASSWORD}" '{username:$u,password:$p}')" \
    "${ARCHIVIST_URL}/api/auth/login")"
  CSRF_TOKEN="$(echo "${response}" | jq -r .csrf_token)"
  jq --arg t "${CSRF_TOKEN}" '.csrf_token = $t' "${STATE_FILE}" > "${STATE_FILE}.tmp" && mv "${STATE_FILE}.tmp" "${STATE_FILE}"
  chmod 600 "${STATE_FILE}"
}

api() {
  local method="$1" path="$2" body="${3-}"
  local args=(-fsS -b "${COOKIE_JAR}" -X "${method}" -H "X-CSRF-Token: ${CSRF_TOKEN}")
  if [[ -n "${body}" ]]; then
    args+=(-H 'Content-Type: application/json' -d "${body}")
  fi
  curl "${args[@]}" "${ARCHIVIST_URL}${path}"
}

refresh_session

# --- Phase 1: sync + queue + drain --------------------------------------
log "Phase 1: manual_review pipeline"
log "PUT /api/workflow/mode -> manual_review"
api PUT /api/workflow/mode '{"mode":"manual_review"}' >/dev/null

log "POST /api/paperless/sync-metadata"
api POST /api/paperless/sync-metadata '{}' | jq -c '.' || true

# NOTE — known v1.4.x bug in archivist-api/src/main.rs::queue_full_batch():
#
#   for stage in settings.workflow.enabled_stages {
#       queue_missing_stage(..., stage, ...);   // creates single-stage runs
#   }
#
# It iterates the enabled stages and creates one single-stage run per (doc,
# stage), so a single /api/batches/full call only queues OCR-only runs first.
# The fix is to call queue_missing_pipeline(enabled_stages, ...) instead, which
# creates ONE multi-stage run per doc. Tracked separately — for the e2e drive
# we work around it by calling /api/batches/full TWICE: once seeds OCR-only
# runs, after OCR drains the second call picks up the docs missing metadata
# and seeds metadata-only runs.
log "POST /api/batches/full (OCR pass)"
api POST /api/batches/full '{}' | jq -c '.' || true

log "polling dashboard until OCR pass drains (30 min budget — vision OCR is slow on Ollama) ..."
# /api/dashboard/live exposes `active_jobs` + `active_runs` as arrays. The
# pipeline is drained iff both arrays are empty for two consecutive samples
# (avoids a false-positive between stage transitions where the next job is not
# yet claimed).
DEADLINE=$(( $(date +%s) + 1800 ))
EMPTY_STREAK=0
LAST_SNAPSHOT=""
while [[ $(date +%s) -lt ${DEADLINE} ]]; do
  STATS="$(api GET /api/dashboard/live)"
  ACTIVE_JOBS="$(echo "${STATS}" | jq '.active_jobs | length')"
  ACTIVE_RUNS="$(echo "${STATS}" | jq '.active_runs | length')"
  RECENT_FAILS="$(echo "${STATS}" | jq '.recent_failures | length')"
  STAGE_BREAKDOWN="$(echo "${STATS}" | jq -c '.active_jobs | group_by(.stage) | map({stage: .[0].stage, count: length})' 2>/dev/null || echo '[]')"
  SUMMARY="active_jobs=${ACTIVE_JOBS} active_runs=${ACTIVE_RUNS} recent_failures=${RECENT_FAILS} stages=${STAGE_BREAKDOWN}"
  if [[ "${SUMMARY}" != "${LAST_SNAPSHOT}" ]]; then
    log "  ${SUMMARY}"
    LAST_SNAPSHOT="${SUMMARY}"
  fi
  if [[ "${ACTIVE_JOBS}" == "0" && "${ACTIVE_RUNS}" == "0" ]]; then
    EMPTY_STREAK=$((EMPTY_STREAK + 1))
    if [[ ${EMPTY_STREAK} -ge 2 ]]; then
      log "pipeline drained"
      break
    fi
  else
    EMPTY_STREAK=0
  fi
  sleep 10
done

# Workaround for the queue_full_batch bug — second pass picks up docs that
# completed OCR but are still missing the Metadata stage.
log "POST /api/batches/full (Metadata pass)"
api POST /api/batches/full '{}' | jq -c '.' || true

log "polling dashboard until Metadata pass drains ..."
DEADLINE=$(( $(date +%s) + 600 ))
EMPTY_STREAK=0
LAST_SNAPSHOT=""
while [[ $(date +%s) -lt ${DEADLINE} ]]; do
  STATS="$(api GET /api/dashboard/live)"
  ACTIVE_JOBS="$(echo "${STATS}" | jq '.active_jobs | length')"
  ACTIVE_RUNS="$(echo "${STATS}" | jq '.active_runs | length')"
  STAGE_BREAKDOWN="$(echo "${STATS}" | jq -c '.active_jobs | group_by(.stage) | map({stage: .[0].stage, count: length})' 2>/dev/null || echo '[]')"
  SUMMARY="active_jobs=${ACTIVE_JOBS} active_runs=${ACTIVE_RUNS} stages=${STAGE_BREAKDOWN}"
  if [[ "${SUMMARY}" != "${LAST_SNAPSHOT}" ]]; then
    log "  ${SUMMARY}"
    LAST_SNAPSHOT="${SUMMARY}"
  fi
  if [[ "${ACTIVE_JOBS}" == "0" && "${ACTIVE_RUNS}" == "0" ]]; then
    EMPTY_STREAK=$((EMPTY_STREAK + 1))
    if [[ ${EMPTY_STREAK} -ge 2 ]]; then
      log "metadata pipeline drained"
      break
    fi
  else
    EMPTY_STREAK=0
  fi
  sleep 10
done

log "fetching review queue ..."
REVIEWS_JSON="$(api GET '/api/reviews?status=pending&limit=200')"
REVIEW_COUNT="$(echo "${REVIEWS_JSON}" | jq '.items | length')"
log "pending reviews: ${REVIEW_COUNT}"
echo "${REVIEWS_JSON}" | jq -c '.items[] | {id, stage, document_id: .paperless_document_id, summary: .summary}' | head -30 >&2

# Approve each pending review item one at a time.
log "approving all pending review items ..."
APPROVED=0
for id in $(echo "${REVIEWS_JSON}" | jq -r '.items[].id'); do
  if api POST "/api/reviews/${id}/approve" '{}' >/dev/null 2>&1; then
    APPROVED=$((APPROVED + 1))
  else
    log "  failed to approve ${id}"
  fi
done
log "approved ${APPROVED}/${REVIEW_COUNT} review items"

log "Phase 1 done"

# --- Phase 2: full_auto -------------------------------------------------
log "Phase 2: full_auto pipeline"
log "PUT /api/workflow/mode -> full_auto"
api PUT /api/workflow/mode '{"mode":"full_auto"}' >/dev/null

# Verify dashboard shows the workflow mode flipped.
api GET /api/workflow/mode 2>/dev/null | jq -c '.' || true

# Pick one of the existing local docs, sync it again and confirm full_auto
# applies the metadata patch without putting it on the review queue. We do this
# by clearing one document's metadata_status back to 'unknown' and re-queueing
# the run via /api/batches/full (which picks up only docs missing a stage).
LOCAL_DOC_ID="$(docker exec compose-postgres-1 psql -U archivist -d archivist -t -A -c "
  select paperless_document_id from document_inventory
   where metadata_status = 'succeeded'
   order by paperless_document_id desc limit 1;
")"

if [[ -n "${LOCAL_DOC_ID}" && "${LOCAL_DOC_ID}" != "(no" ]]; then
  log "selecting doc ${LOCAL_DOC_ID} for full_auto re-run"
  docker exec compose-postgres-1 psql -U archivist -d archivist -c "
    update document_inventory set metadata_status = 'unknown'
     where paperless_document_id = ${LOCAL_DOC_ID};
  " >/dev/null
  log "POST /api/batches/full (full_auto)"
  api POST /api/batches/full '{}' | jq -c '.' || true

  # Wait briefly for the new run + observe what landed.
  log "watching dashboard for ~2 min ..."
  DEADLINE=$(( $(date +%s) + 120 ))
  while [[ $(date +%s) -lt ${DEADLINE} ]]; do
    STATS="$(api GET /api/dashboard/live)"
    ACTIVE="$(echo "${STATS}" | jq '.active_jobs | length')"
    if [[ "${ACTIVE}" == "0" ]]; then
      log "full_auto pipeline drained"
      break
    fi
    sleep 5
  done

  POST_REVIEWS="$(api GET '/api/reviews?status=pending&limit=200' | jq '.items | length')"
  log "pending review items after full_auto: ${POST_REVIEWS}"
  AUDIT_AUTOPILOT="$(docker exec compose-postgres-1 psql -U archivist -d archivist -t -A -c "
    select count(*) from audit_events
     where event_type like '%apply%' and actor_type = 'worker'
       and paperless_document_id = ${LOCAL_DOC_ID}
       and created_at > now() - interval '10 min';
  ")"
  log "autopilot apply audit_events for doc ${LOCAL_DOC_ID}: ${AUDIT_AUTOPILOT}"
fi

log "Phase 2 done"
echo
log "Run scripts/dev-local-verify.sh to assert v1.4.x correctness invariants"
