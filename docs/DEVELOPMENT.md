# Paperless Archivist Development Guide

This guide describes the current development workflow for the Rust backend,
worker, PostgreSQL migrations, and React frontend.

## Prerequisites

- Rust toolchain matching `Cargo.toml` edition support
- Docker with Compose
- pnpm
- PostgreSQL 18 for local integration runs
- optional local Ollama

The fastest full-system loop is Docker Compose:

```bash
docker compose --env-file deploy/compose/.env.example -f deploy/compose/docker-compose.yml up --build -d
```

For production-like local work, copy `.env.example` and set real secrets:

```bash
cp deploy/compose/.env.example deploy/compose/.env
$EDITOR deploy/compose/.env
docker compose --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build -d
```

## Common Commands

Rust:

```bash
cargo fmt --check
cargo test
cargo test -p archivist-core
cargo run -p archivist-api
cargo run -p archivist-worker
```

Frontend:

```bash
pnpm --dir frontend install
pnpm --dir frontend generate:client
pnpm --dir frontend typecheck
pnpm --dir frontend build
pnpm --dir frontend dev
```

Compose:

```bash
docker compose --env-file deploy/compose/.env.example -f deploy/compose/docker-compose.yml ps
docker compose --env-file deploy/compose/.env.example -f deploy/compose/docker-compose.yml logs -f api worker
docker compose --env-file deploy/compose/.env.example -f deploy/compose/docker-compose.yml down
```

## Backend Development

The API is responsible for:

- authentication and authorization
- settings, prompts, users, sessions, API tokens
- Paperless sync and manual job creation
- review decisions and apply operations
- dashboard analytics and metrics
- static frontend serving

The worker is responsible for:

- polling trigger tags
- claiming jobs from PostgreSQL
- calling Paperless and AI providers
- storing AI artifacts
- validating model output
- creating review items or applying patches

Keep long-running AI/Paperless work in the worker. API endpoints should create
jobs or return persisted state.

## Database and Migrations

Migrations live in `migrations/` and are run by API and worker startup through
SQLx. PostgreSQL 18 is required. The initial migration fails clearly on older
versions.

Rules for migrations:

- never reuse a migration version number
- append new migrations with the next numeric prefix
- use `uuidv7()` for new Archivist primary keys where practical
- keep Paperless integration state in Archivist tables only
- never write to the Paperless database directly

If a local Compose database reports a modified migration checksum, check
`_sqlx_migrations`; the usual cause is accidentally reusing an existing
migration number. Add a new migration instead of editing an applied one.

## Frontend Development

The frontend is a React + TypeScript Vite app. It uses:

- handwritten API wrapper in `frontend/src/api/client.ts`
- generated OpenAPI types in `frontend/src/api/schema.ts`
- Recharts for dashboard analytics
- lucide-react for icons
- CSS in `frontend/src/styles/app.css`

Frontend implementation guidelines:

- keep operational screens dense and scannable
- use existing colors and 8px-or-less card radius
- keep settings and dashboard controls usable on mobile
- preserve existing API-only communication; no frontend calls to Paperless or AI providers
- regenerate OpenAPI types after contract changes

## API Contract Changes

When changing backend response shapes:

1. Update shared Rust types where appropriate.
2. Update [openapi/openapi.yaml](../openapi/openapi.yaml).
3. Run `pnpm --dir frontend generate:client`.
4. Update `frontend/src/api/client.ts`.
5. Run `pnpm --dir frontend typecheck`.

## Testing Expectations

Run before committing:

```bash
cargo fmt --check
cargo test
pnpm --dir frontend typecheck
pnpm --dir frontend build
```

For deployment-affecting work, also run:

```bash
docker compose --env-file deploy/compose/.env.example -f deploy/compose/docker-compose.yml up --build -d
curl http://127.0.0.1:8080/healthz
curl http://127.0.0.1:8080/readyz
```

The frontend build may warn about chunk size because Recharts is included. That
warning does not fail the build.

## Local Login

With `deploy/compose/.env.example`, the bootstrap login is:

```text
username: admin
password: change-me-admin-password
```

Use a real `.env` file with stronger secrets for non-throwaway environments.

## Troubleshooting

- `Paperless token is not configured`: configure Paperless in the Settings UI.
- `Ollama is reachable but model was not listed`: pull the selected model or choose an installed model.
- `migration N was previously applied but has been modified`: do not edit an applied migration; create the next migration number.
- `ARCHIVIST_ADMIN_PASSWORD is not set`: set it for first boot, enable OIDC, or keep an existing user in the DB.
- frontend API calls return HTML: verify the API route begins with `/api` and the backend is running.
