# Implementation Plan

Status: historical build plan
Purpose: record the concrete build order that was used for the MVP.

The implemented system is documented in [`PROJECT_OVERVIEW.md`](PROJECT_OVERVIEW.md),
[`OPERATIONS.md`](OPERATIONS.md), [`DEVELOPMENT.md`](DEVELOPMENT.md), and
[`API_REFERENCE.md`](API_REFERENCE.md). This file is retained as implementation
history and as context for future phases.

This plan assumes the architecture decisions are accepted:

- Rust backend, worker, domain, integration, and CLI code
- React + TypeScript frontend
- PostgreSQL 18 only
- Paperless-ngx REST API only
- Ollama first, OpenAI/Claude/OpenAI-compatible providers next
- login/RBAC/audit required
- Kubernetes-first, Docker Compose-supported

## 1. MVP Definition

The MVP is a usable system when a user can:

1. Deploy with Docker Compose or Kubernetes.
2. Create/login as an admin.
3. Configure Paperless and Ollama from the UI.
4. Sync Paperless document inventory.
5. See which documents still need OCR and tagging.
6. Queue OCR/tagging/full-pipeline work for one document or many documents.
7. Process OCR and tagging via worker jobs.
8. Review suggestions or run autopilot where validation allows it.
9. Apply changes through the Paperless API.
10. See completion tags, run state, failures, and audit logs.

## 2. Repository Structure to Create

```text
Cargo.toml
crates/
  archivist-api/
  archivist-worker/
  archivist-core/
  archivist-db/
  archivist-paperless/
  archivist-ai/
  archivist-ocr/
  archivist-config/
frontend/
  package.json
  pnpm-lock.yaml
  src/
migrations/
deploy/
  compose/
  kubernetes/
docs/
tests/
  fixtures/
```

## 3. Phase 0: Tooling Foundation

Deliverables:

- Rust workspace
- crate skeletons
- React/Vite/pnpm frontend skeleton
- shared formatting/linting commands
- CI pipeline
- Dockerfile
- Docker Compose skeleton
- Kubernetes skeleton

Rust baseline:

- `tokio`
- `axum`
- `serde`
- `serde_json`
- `thiserror`
- `tracing`
- `reqwest`
- `sqlx`
- `utoipa` or equivalent OpenAPI generation approach

Frontend baseline:

- React
- TypeScript
- Vite
- pnpm
- TanStack Query
- TanStack Table
- React Hook Form
- Zod
- generated OpenAPI client

Acceptance:

- `cargo test` runs
- frontend typecheck/build runs
- container builds
- Compose starts API, worker, PostgreSQL 18
- API exposes `/healthz` and `/readyz`

## 4. Phase 1: PostgreSQL 18 Schema

Create SQLx migrations for:

- users
- roles/permissions or role assignments
- sessions
- api_tokens
- settings
- secret references
- ai_providers
- prompts
- paperless metadata cache
- document_inventory
- pipeline_runs
- jobs
- ai_artifacts
- review_items
- audit_events

Mandatory PostgreSQL 18 usage:

- `uuidv7()` defaults for Archivist IDs
- PostgreSQL 18 in CI
- dedicated application role
- SCRAM auth in deployment docs

Acceptance:

- migrations run on PostgreSQL 18
- schema cannot run on older PostgreSQL without failing clearly
- basic repository functions are covered by integration tests

## 5. Phase 2: Security Foundation

Implement:

- local users
- admin bootstrap
- Argon2id password hashing
- login/logout
- server-side sessions
- CSRF protection
- RBAC middleware
- scoped API tokens
- audit event writer
- secure headers

Initial roles:

- `viewer`
- `reviewer`
- `operator`
- `admin`
- `auditor`

Acceptance:

- UI is not usable without login
- admin can create users
- settings endpoints require admin
- review endpoints require reviewer or admin
- batch/job endpoints require operator or admin
- all auth/security-relevant actions produce audit events

## 6. Phase 3: API Contract and Frontend Shell

Implement:

- OpenAPI generation or checked OpenAPI spec
- generated TypeScript client
- React app shell
- login screen
- navigation layout
- initial dashboard screen
- initial settings screen
- initial review screen
- initial audit screen

Acceptance:

- frontend uses generated client
- no direct Paperless/provider calls from frontend
- stale OpenAPI client fails CI
- React app is served by Rust API container

## 7. Phase 4: Settings and Provider Configuration

Implement UI/API for:

- Paperless base URL/public URL
- Paperless token secret reference
- Ollama base URL
- text model
- vision model
- OpenAI provider config
- Claude provider config
- OpenAI-compatible provider config
- per-stage model overrides
- workflow tag names
- OCR page limits
- review/autopilot mode

Acceptance:

- admin can configure providers from UI
- provider connection test works
- secrets are not displayed after save
- settings changes are audited

## 8. Phase 5: Paperless Integration and Inventory

Implement:

- Paperless REST client
- pagination
- retries/timeouts
- metadata sync
- workflow tag ensure/create
- document inventory sync
- backlog calculation
- backlog dashboard API/UI

Inventory must show:

- total documents
- missing OCR
- missing tagging
- failed
- waiting review
- running
- complete
- never processed

Acceptance:

- user can sync Paperless inventory
- user can see missing OCR/tagging counts
- workflow tags are created if missing
- no direct Paperless DB writes exist

## 9. Phase 6: Job Queue and Worker Runtime

Implement:

- job creation
- job leasing with `FOR UPDATE SKIP LOCKED`
- worker concurrency
- retry/backoff
- lease recovery
- failed job handling
- run state transitions
- audit events for job actions

Acceptance:

- multiple workers can run without duplicate job execution
- worker restart does not lose queued/running jobs
- failed jobs are visible and retryable

## 10. Phase 7: AI Provider Layer

Implement:

- provider trait for text/chat
- provider trait for vision
- Ollama text provider
- Ollama vision provider
- response normalization
- provider timeouts
- provider error redaction

Then add:

- OpenAI provider
- Anthropic Claude provider
- OpenAI-compatible provider

Acceptance:

- provider tests run against mocks
- provider config test endpoint works
- raw provider responses are stored/redacted according to settings

## 11. Phase 8: OCR Stage

Implement:

- download original document from Paperless
- render first N pages to images
- call vision model
- normalize OCR output
- validate non-empty output
- store OCR artifact
- update Paperless document `content`
- add `ai-processed-ocr`
- remove `ai-ocr` or matching trigger tag

Acceptance:

- one document can be OCR-processed end to end
- failed OCR leaves a visible failure state
- successful OCR sets completion tag
- document text is not logged by default

## 12. Phase 9: Tagging Stage

Implement:

- get allowed Paperless tags
- exclude workflow tags
- generate tag suggestions
- validate max tag count
- validate existing tags in autopilot
- store suggestion
- review/edit/apply flow
- optional old tag removal/preservation strategy
- add `ai-processed-tagging`
- remove `ai-tags` or matching trigger tag

Acceptance:

- one document can be tagged end to end
- review mode works
- autopilot applies only valid suggestions
- new tags require review by default

## 13. Phase 10: Batch Processing

Implement:

- queue all missing OCR
- queue all missing tagging
- queue full pipeline for incomplete documents
- filters by document ID/date/tag/stage/status
- pause/resume batch
- retry failed subset
- progress UI

Acceptance:

- user can queue all missing OCR jobs
- user can queue all missing tagging jobs
- UI shows progress and remaining work
- failures do not stop unrelated documents

## 14. Phase 11: Deployment Hardening

Docker Compose:

- API/UI service
- worker service
- PostgreSQL 18 service
- optional Ollama service
- named volumes
- secret file support

Production deployment package:

- API/UI workload
- worker workload
- service and ingress owned by the deployment environment
- runtime secrets owned by the deployment environment
- NetworkPolicy where the platform supports it
- probes
- resource requests/limits
- non-root security context

Acceptance:

- Compose deployment works from a clean checkout
- production manifests can be generated or maintained outside the public source
  tree using the same container image and runtime settings
- both use the same runtime UI configuration model

## 15. Phase 12: Observability and Release Readiness

Implement:

- structured JSON logs
- Prometheus metrics
- audit export
- job/run metrics
- provider latency metrics
- backup/restore docs
- security review checklist
- dependency scanning
- image scanning

Acceptance:

- operators can diagnose failures from UI/logs/metrics
- release checklist is documented
- MVP acceptance criteria in `SOLUTION_DESIGN.md` are all met

## 16. Development Rules

- Rust backend validation is authoritative.
- Frontend never talks directly to Paperless or AI providers.
- All Paperless changes go through Paperless REST API.
- Every state-changing security/admin/review/apply action writes audit events.
- No full document text in normal logs.
- No secret displayed after save.
- New tags require review unless an admin explicitly changes policy.
- Completion tags are set by code, not by prompts.
- All jobs are resumable and idempotent.

## 17. Suggested First Pull Requests

1. `chore: create rust workspace and frontend skeleton`
2. `chore: add postgres 18 compose environment`
3. `feat: add config loading and health endpoints`
4. `feat: add database migrations for auth and jobs`
5. `feat: add login sessions and RBAC middleware`
6. `feat: add OpenAPI generation and frontend client`
7. `feat: add Paperless client and metadata sync`
8. `feat: add document inventory dashboard`
9. `feat: add worker job leasing`
10. `feat: add Ollama provider and tagging stage`
11. `feat: add OCR stage`
12. `feat: add review queue and apply flow`
