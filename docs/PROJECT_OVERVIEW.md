# Paperless Archivist Project Overview

Paperless Archivist is a companion service for Paperless-ngx. Paperless remains
the system of record for documents and metadata; Archivist adds AI-assisted
processing, review controls, runtime configuration, analytics, and auditability.

## Runtime Architecture

```text
React UI
  |
  v
Rust/Axum API  <---->  PostgreSQL 18
  |                        ^
  |                        |
  v                        v
Paperless-ngx REST API   Rust/Tokio Worker
                           |
                           v
             Ollama / Ollama Cloud / OpenAI / Anthropic / OpenAI-compatible
```

The frontend never talks directly to Paperless or AI providers. All writes to
Paperless go through the Paperless REST API. PostgreSQL stores Archivist state:
settings, encrypted secret references, users, sessions, inventory cache, runs,
jobs, review items, AI artifacts, chat sessions, dashboard snapshots, and audit
events.

## Main Capabilities

- Login, RBAC, CSRF, sessions, API tokens, and local user management.
- Runtime settings for Paperless, AI providers, prompts, workflow mode, and security operations.
- Paperless metadata and document inventory sync.
- OCR and vision processing through model providers.
- Tagging, title, correspondent, document type, document date, and custom field
  suggestions.
- Local document language detection with BCP-47 tags and language-aware prompts.
- Review queue for approve/reject/edit workflows.
- Document chat/RAG over Paperless document content with stored sources.
- Three workflow modes: manual review, auto-select with review, and full autopilot after validation.
- Completion tags and trigger tag cleanup in Paperless.
- Resumable worker jobs with leases, retries, idempotent apply behavior, and audit events.
- Dashboard analytics with selectable time ranges, snapshots, and operational charts.
- Prometheus metrics for jobs, reviews, runs, and audits.
- Docker Compose for local operation and container images for production
  packaging.

## Repository Layout

```text
crates/
  archivist-api/        Axum API, auth middleware, UI serving, endpoint orchestration
  archivist-worker/     background job processing and Paperless apply flow
  archivist-core/       shared domain types, validation, workflow tags, settings
  archivist-db/         SQLx repositories, migrations access, job leasing
  archivist-paperless/  Paperless REST client
  archivist-ai/         Ollama/OpenAI-compatible/Anthropic clients and prompt parsing
  archivist-ocr/        PDF/image extraction helpers
  archivist-config/     environment-driven runtime config
frontend/
  React + TypeScript app, generated OpenAPI schema, Recharts dashboard
migrations/
  PostgreSQL 18 SQLx migrations
openapi/
  OpenAPI contract used to generate frontend types
deploy/
  Docker Compose files for local operation
docs/
  architecture, operation, security, product, and development documentation
```

## Processing Flow

1. An operator syncs Paperless metadata or a worker sees configured trigger tags.
2. The API creates a `pipeline_run` and ordered `jobs` for selected stages.
3. Workers claim jobs with PostgreSQL row locking and leases.
4. The worker fetches document content/original files through the Paperless REST API.
5. The worker calls the configured AI provider and stores an AI artifact.
6. Rust validation bounds and normalizes model output.
7. Standard metadata suggestions include evidence, confidence, and current
   Paperless values for review.
8. In `manual_review` or `auto_select_review` mode, suggestions go to `review_items`.
9. In `full_auto` mode, valid suggestions are applied through Paperless REST.
10. Successful apply operations add completion tags and remove trigger tags.
11. Every meaningful state change writes an audit event.

## Document Chat Flow

1. A user creates or opens a chat session.
2. The API retrieves candidate documents from the local inventory cache.
3. Candidate content is fetched through the Paperless REST API.
4. Rust domain logic builds bounded source snippets and a citation-oriented prompt.
5. The configured default text provider generates the answer.
6. User messages, assistant messages, source snippets, provider metadata, and
   audit events are stored in PostgreSQL.

The frontend never sends document content directly to providers. It only calls
Archivist `/api/chat/*` endpoints.

## Dashboard Analytics

The dashboard combines live aggregate counts with historical snapshots:

- current counts come from `document_inventory`
- throughput and run activity come from `jobs` and `pipeline_runs`
- backlog trend comes from `dashboard_snapshots`
- snapshots are recorded when `/api/dashboard` is queried and the newest
  snapshot is older than five minutes
- supported ranges: `24h`, `7d`, `30d`, `90d`, `12m`, `all`

Backlog history starts when the snapshot migration is deployed. Older history is
not reconstructed.

## OpenAPI Boundary

The OpenAPI file at [openapi/openapi.yaml](../openapi/openapi.yaml) is the
contract between backend and frontend. After changing it, regenerate the
frontend schema:

```bash
pnpm --dir frontend generate:client
```

The handwritten `frontend/src/api/client.ts` exposes a small typed wrapper used
by the React app.
