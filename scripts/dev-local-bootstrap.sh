#!/usr/bin/env bash
# Bootstrap the dev-local stack to a known-good baseline.
#
# Steps:
#   1. Wait for local Paperless-ngx to finish first-run setup.
#   2. Mint a local Paperless API token for the archivist to use.
#   3. Log in to the archivist API as admin, capture session + CSRF.
#   4. PUT runtime settings so the archivist points at local Paperless,
#      host Ollama, and a non-crashing vision model.
#
# Idempotent: re-running just refreshes the runtime settings.
#
# Required env (sourced from deploy/compose/.env.dev-local by default):
#   ARCHIVIST_ADMIN_USERNAME, ARCHIVIST_ADMIN_PASSWORD
#   PAPERLESS_ADMIN_USER, PAPERLESS_ADMIN_PASSWORD
#
# Writes:
#   scripts/.dev-local-state.json — captured tokens/cookies for follow-up scripts.
#   This file is gitignored.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
ENV_FILE="${ENV_FILE:-${REPO_ROOT}/deploy/compose/.env.dev-local}"
STATE_FILE="${STATE_FILE:-${SCRIPT_DIR}/.dev-local-state.json}"

ARCHIVIST_URL="${ARCHIVIST_URL:-http://127.0.0.1:18080}"
PAPERLESS_URL_LOCAL="${PAPERLESS_URL_LOCAL:-http://127.0.0.1:8000}"
PAPERLESS_URL_INTERNAL="${PAPERLESS_URL_INTERNAL:-http://paperless-ngx:8000}"
OLLAMA_URL="${OLLAMA_URL:-http://host.docker.internal:11434}"

# Default to fast, non-crashing models from host Ollama (see `ollama list`).
# qwen2.5vl:7b (6 GB) is ~5× faster than qwen3-vl:32b on a 64 GB Mac for OCR
# while staying stable. glm-ocr is excluded outright because of the prod
# GGML_ASSERT crashes that motivated this dev stack in the first place.
TEXT_MODEL="${TEXT_MODEL:-qwen3:30b}"
VISION_MODEL="${VISION_MODEL:-qwen2.5vl:7b}"

if [[ -f "${ENV_FILE}" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "${ENV_FILE}"
  set +a
fi

ARCHIVIST_ADMIN_USERNAME="${ARCHIVIST_ADMIN_USERNAME:?required}"
ARCHIVIST_ADMIN_PASSWORD="${ARCHIVIST_ADMIN_PASSWORD:?required}"
PAPERLESS_ADMIN_USER="${PAPERLESS_ADMIN_USER:?required}"
PAPERLESS_ADMIN_PASSWORD="${PAPERLESS_ADMIN_PASSWORD:?required}"

log() { printf '[bootstrap] %s\n' "$*" >&2; }

# --- 1. Wait for Paperless to be ready ----------------------------------
log "waiting for Paperless-ngx at ${PAPERLESS_URL_LOCAL} ..."
for i in $(seq 1 60); do
  if curl -fsS -o /dev/null -w "%{http_code}" "${PAPERLESS_URL_LOCAL}/api/" 2>/dev/null | grep -qE '^(200|301|302|401|403)$'; then
    log "Paperless is up (probe ${i})"
    break
  fi
  sleep 2
  if [[ ${i} -eq 60 ]]; then
    log "Paperless did not become ready after 120s"; exit 1
  fi
done

# --- 2. Mint Paperless API token via /api/token/ -------------------------
log "minting Paperless API token ..."
PAPERLESS_TOKEN="$(curl -fsS -X POST \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg u "${PAPERLESS_ADMIN_USER}" --arg p "${PAPERLESS_ADMIN_PASSWORD}" \
        '{username:$u,password:$p}')" \
  "${PAPERLESS_URL_LOCAL}/api/token/" | jq -r .token)"

if [[ -z "${PAPERLESS_TOKEN}" || "${PAPERLESS_TOKEN}" == "null" ]]; then
  log "failed to mint Paperless token"; exit 1
fi
log "Paperless token: ${PAPERLESS_TOKEN:0:8}…"

# --- 3. Archivist login --------------------------------------------------
log "logging into archivist as ${ARCHIVIST_ADMIN_USERNAME} ..."
COOKIE_JAR="$(mktemp)"
trap 'rm -f "${COOKIE_JAR}"' EXIT

LOGIN_RESPONSE="$(curl -fsS -c "${COOKIE_JAR}" -X POST \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg u "${ARCHIVIST_ADMIN_USERNAME}" --arg p "${ARCHIVIST_ADMIN_PASSWORD}" \
        '{username:$u,password:$p}')" \
  "${ARCHIVIST_URL}/api/auth/login")"

CSRF_TOKEN="$(echo "${LOGIN_RESPONSE}" | jq -r .csrf_token)"
if [[ -z "${CSRF_TOKEN}" || "${CSRF_TOKEN}" == "null" ]]; then
  log "archivist login failed: ${LOGIN_RESPONSE}"; exit 1
fi
log "archivist session OK; csrf=${CSRF_TOKEN:0:8}…"

# --- 4. Read current settings and patch ----------------------------------
log "fetching current archivist settings ..."
CURRENT_SETTINGS="$(curl -fsS -b "${COOKIE_JAR}" "${ARCHIVIST_URL}/api/settings")"

UPDATED_SETTINGS="$(echo "${CURRENT_SETTINGS}" | jq \
  --arg base "${PAPERLESS_URL_INTERNAL}" \
  --arg pub  "${PAPERLESS_URL_LOCAL}" \
  --arg ollama "${OLLAMA_URL}" \
  --arg text "${TEXT_MODEL}" \
  --arg vision "${VISION_MODEL}" \
  '
  .paperless.base_url = $base
  | .paperless.public_url = $pub
  | .paperless.timeout_seconds = 60
  # `paperless_client_from_settings` (archivist-api/src/main.rs) picks the active
  # archive profile’s base_url FIRST and only falls back to the top-level value
  # when no profile matches. The normalizer stamps the legacy default profile on
  # first save, so we must also rewrite the active profile to keep things in
  # sync after every settings update.
  | .paperless.active_archive = "default"
  | .paperless.archive_profiles = [{
      "name": "default",
      "base_url": $base,
      "token_secret_id": null,
      "enabled": true
    }]
  | .ai.default_provider = "ollama"
  | .ai.ollama_base_url = $ollama
  | .ai.default_text_model = $text
  | .ai.default_vision_model = $vision
  | .ai.external_provider_warning_acknowledged = true
  | .workflow.mode = "manual_review"
  | .workflow.paused = false
  | .workflow.dry_run = false
  | .workflow.enabled_stages = ["ocr","metadata"]
  | .ocr.language_hint = "deu+eng"
  | .metadata.allow_new_correspondents = true
  | .metadata.allow_new_document_types = true
  | .tagging.allow_new_tags = true
  ')"

log "PUT /api/settings ..."
PUT_BODY="$(jq -n \
  --argjson settings "${UPDATED_SETTINGS}" \
  --arg token "${PAPERLESS_TOKEN}" \
  '{settings:$settings, paperless_token:$token}')"

curl -fsS -b "${COOKIE_JAR}" -X PUT \
  -H 'Content-Type: application/json' \
  -H "X-CSRF-Token: ${CSRF_TOKEN}" \
  -d "${PUT_BODY}" \
  "${ARCHIVIST_URL}/api/settings" >/dev/null

log "verifying with /api/settings/test-paperless ..."
TEST_RESP="$(curl -fsS -b "${COOKIE_JAR}" -X POST \
  -H "X-CSRF-Token: ${CSRF_TOKEN}" \
  -H 'Content-Type: application/json' \
  -d '{}' \
  "${ARCHIVIST_URL}/api/settings/test-paperless" || true)"
log "test-paperless: ${TEST_RESP}"

# --- 5. Persist state for follow-up scripts ------------------------------
jq -n \
  --arg archivist_url "${ARCHIVIST_URL}" \
  --arg paperless_url "${PAPERLESS_URL_LOCAL}" \
  --arg paperless_token "${PAPERLESS_TOKEN}" \
  --arg csrf "${CSRF_TOKEN}" \
  --argjson cookies "$(jq -n '[]')" \
  '{archivist_url:$archivist_url, paperless_url:$paperless_url,
    paperless_token:$paperless_token, csrf_token:$csrf}' \
  > "${STATE_FILE}"

# Persist the cookie jar in the same dir for the seed script.
cp "${COOKIE_JAR}" "${SCRIPT_DIR}/.dev-local-cookies.txt"
chmod 600 "${STATE_FILE}" "${SCRIPT_DIR}/.dev-local-cookies.txt"

log "bootstrap complete. state -> ${STATE_FILE}"
