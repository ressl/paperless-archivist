#!/usr/bin/env bash
# Thin shim that ensures a Python venv with `requests` is available, then runs
# scripts/seed-from-prod.py. PEP-668 forbids system-pip on macOS Homebrew so we
# stash the venv under a stable path the user can rm at will.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV_DIR="${SEED_VENV_DIR:-${XDG_CACHE_HOME:-${HOME}/.cache}/paperless-archivist/seed-venv}"

if [[ ! -x "${VENV_DIR}/bin/python" ]]; then
  echo "[dev-local-seed] bootstrapping venv at ${VENV_DIR} ..."
  python3 -m venv "${VENV_DIR}"
  "${VENV_DIR}/bin/pip" install --quiet --upgrade pip
  "${VENV_DIR}/bin/pip" install --quiet requests
fi

exec "${VENV_DIR}/bin/python" "${SCRIPT_DIR}/seed-from-prod.py" "$@"
