#!/bin/bash
# Auto-runs on first Postgres init (single $PGDATA). Creates the second database
# Paperless-ngx needs alongside the archivist database. Keeping both apps in one
# PG instance is fine for a dev box and removes a whole service from the stack.
set -euo pipefail

PAPERLESS_DBNAME="${PAPERLESS_DBNAME:-paperless}"
PAPERLESS_DBUSER="${PAPERLESS_DBUSER:-paperless}"
PAPERLESS_DBPASS="${PAPERLESS_DBPASS:?PAPERLESS_DBPASS must be set}"

psql -v ON_ERROR_STOP=1 \
     --username "${POSTGRES_USER}" \
     --dbname "${POSTGRES_DB}" <<-EOSQL
  DO \$\$
  BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '${PAPERLESS_DBUSER}') THEN
      CREATE ROLE "${PAPERLESS_DBUSER}" LOGIN PASSWORD '${PAPERLESS_DBPASS}';
    END IF;
  END
  \$\$;
EOSQL

# CREATE DATABASE cannot be wrapped in DO/PL/pgSQL; use a separate, idempotent
# check via psql's \gexec to avoid failing on rerun.
psql -v ON_ERROR_STOP=1 \
     --username "${POSTGRES_USER}" \
     --dbname "${POSTGRES_DB}" <<-EOSQL
  SELECT 'CREATE DATABASE "${PAPERLESS_DBNAME}" OWNER "${PAPERLESS_DBUSER}"'
   WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = '${PAPERLESS_DBNAME}')
  \gexec
EOSQL
