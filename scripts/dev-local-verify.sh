#!/usr/bin/env bash
# Verify the v1.4.x correctness invariants against the dev-local stack.
#
# These are the assertions that, if any of them fail, the pipeline did NOT
# behave like v1.4.x. They are the local mirror of the prod-bug acceptance
# checks the operator highlighted:
#
#   1. Every doc that completed the Metadata stage has EXACTLY ONE ai_artifact
#      row for stage=metadata (not six per-field rows).
#   2. No legacy per-field stage (title|tags|correspondent|document_type|
#      document_date|fields) appears in ai_artifacts for runs created after
#      bootstrap.
#   3. Review items, when present, are grouped on the Metadata stage rather
#      than fanned out across legacy stages.
#   4. The dashboard `stage_status` query returns rows for `ocr` AND
#      `metadata` (the v1.4.x default sequence).
#
# Run AFTER ./scripts/dev-local-e2e.sh.
set -euo pipefail

PSQL=(docker exec compose-postgres-1 psql -U archivist -d archivist -t -A)

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }

fail=0
note()  { printf '  %s\n' "$*"; }
check() {
  local label="$1" expected="$2" actual="$3"
  if [[ "${actual}" == "${expected}" ]]; then
    green "PASS  ${label}  (got ${actual})"
  else
    red   "FAIL  ${label}  (expected ${expected}, got ${actual})"
    fail=$((fail + 1))
  fi
}

# 1. exactly-one metadata ai_artifact per pipeline_run.
DUPES=$("${PSQL[@]}" -c "
  with per_run as (
    select run_id, count(*) as n
      from ai_artifacts
     where stage = 'metadata'
     group by run_id
  )
  select coalesce(count(*), 0) from per_run where n > 1;
")
check "exactly-one metadata artifact per pipeline_run" "0" "${DUPES}"

# 2. zero legacy per-field artifacts since the seed completed
LEGACY=$("${PSQL[@]}" -c "
  select count(*) from ai_artifacts
   where stage in ('title','tags','correspondent','document_type','document_date','fields');
")
check "no legacy per-field ai_artifacts" "0" "${LEGACY}"

# 3. review items, when present, are on the Metadata stage (or OCR, never on
#    legacy per-field stages)
LEGACY_REVIEWS=$("${PSQL[@]}" -c "
  select count(*) from review_items
   where stage in ('title','tags','correspondent','document_type','document_date','fields');
")
check "no legacy per-field review_items" "0" "${LEGACY_REVIEWS}"

# 4. v1.5.2 Bug 1 regression: manual-batch runs created by /api/batches/full must
#    carry the full enabled-stages array, NOT single-stage runs.
#    enabled_stages defaults to ["ocr","metadata"], so a run with stages = ["ocr"]
#    or ["metadata"] (and no other stage) is the symptom of the per-stage loop
#    that was replaced by queue_missing_pipeline in v1.5.2.
SINGLE_STAGE_FULL_RUNS=$("${PSQL[@]}" -c "
  select count(*) from pipeline_runs
   where trigger_tag = 'manual-batch'
     and jsonb_array_length(stages) = 1
     and (stages @> '[\"ocr\"]'::jsonb or stages @> '[\"metadata\"]'::jsonb);
")
check "no single-stage manual-batch runs (Bug 1 v1.5.2)" "0" "${SINGLE_STAGE_FULL_RUNS}"

# 5. document_inventory.metadata_status was set by the consolidated stage
NEEDED=$("${PSQL[@]}" -c "
  select count(*) from document_inventory
   where metadata_status in ('succeeded','waiting_review','skipped','not_needed');
")
note "  document_inventory rows with metadata_status set: ${NEEDED}"
if (( NEEDED == 0 )); then
  red "WARN  no docs have metadata_status set — pipeline may not have reached the Metadata stage yet"
fi

# 5. dashboard stage_status — pull live & print, but ALSO note whether the
#    query body itself still returns the legacy 7-row set (an outstanding bug
#    in archivist-db/src/lib.rs::stage_status that needs the prod-fix agent).
echo
echo "Dashboard stage_status (from prod source query):"
STAGE_STATUS_JSON="$(curl -fsS -b "$(dirname "$0")/.dev-local-cookies.txt" http://127.0.0.1:18080/api/dashboard 2>/dev/null \
  | jq -c '.stats.stage_status | map(.stage)')"
echo "  stages returned: ${STAGE_STATUS_JSON}"
if echo "${STAGE_STATUS_JSON}" | grep -q '"metadata"'; then
  green "PASS  dashboard stage_status includes 'metadata' row"
else
  red   "FAIL  dashboard stage_status is missing 'metadata' (KNOWN prod bug in archivist-db/src/lib.rs::stage_status)"
  fail=$((fail + 1))
fi
if echo "${STAGE_STATUS_JSON}" | grep -qE '"title"|"document_type"|"correspondent"|"document_date"|"fields"'; then
  red   "FAIL  dashboard stage_status still includes legacy per-field rows (KNOWN prod bug)"
  fail=$((fail + 1))
else
  green "PASS  no legacy per-field stage_status rows"
fi

echo
if (( fail == 0 )); then
  green "ALL v1.4.x METADATA CHECKS PASSED"
else
  red "${fail} check(s) FAILED"
fi
exit ${fail}
