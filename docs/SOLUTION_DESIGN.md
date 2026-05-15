# Paperless Archivist Solution Design

Status: draft  
Target audience: engineers implementing the first production-ready version  
Primary integration: Paperless-ngx  
Primary runtime: Kubernetes  
Primary language: Rust  
Primary persistence: PostgreSQL 18

Architecture decisions are recorded in
[Architecture Decisions](ARCHITECTURE_DECISIONS.md). The most important early
decisions are:

- the backend, worker, domain logic, and integrations are Rust
- the frontend is React + TypeScript
- PostgreSQL 18 is mandatory
- the API and worker are separate Rust binaries
- frontend and backend have a clean OpenAPI boundary and can ship as one API/UI
  service for the MVP
- Ollama, OpenAI, Anthropic Claude, and OpenAI-compatible APIs are first-class
  AI provider targets
- runtime configuration is managed from the UI after bootstrap
- Kubernetes is the primary production target, Docker Compose is supported
- user login, RBAC, audit logging, and secure defaults are required from the
  first usable release

## 1. Purpose

Paperless Archivist is an AI document intelligence companion for Paperless-ngx.

Paperless-ngx remains the system of record for documents, files, tags,
correspondents, document types, custom fields, users, and permissions. Archivist
adds a deterministic AI processing layer around Paperless-ngx:

- better OCR using vision models
- OCR cleanup
- title generation
- tag selection
- correspondent classification
- document type classification
- custom field extraction
- review-before-apply workflows
- automatic processing based on trigger tags
- audit logs for every AI-driven change

The service must be safe enough for private documents. It must support local AI
providers such as Ollama first, while allowing OpenAI-compatible APIs later.

## 2. Product Principles

1. Paperless-ngx is authoritative.
   Archivist must not bypass Paperless-ngx by writing directly to the Paperless
   database.

2. AI output is untrusted.
   Every model response must be parsed, validated, bounded, and either reviewed
   or applied by explicit policy.

3. Every change is explainable.
   Archivist must store the prompt version, model, input summary/hash, output,
   applied diff, and result status for each processing run.

4. Processing is resumable.
   Restarting the worker must not lose work or duplicate irreversible changes.

5. Local-first by default.
   The default deployment should work with Ollama and without external AI APIs.

6. Clear workflow tags.
   Tags are used as the integration contract with Paperless-ngx. Trigger tags
   request work. Completion tags record finished stages.

## 3. Non-Goals for the First Version

- Replacing Paperless-ngx as a document management system.
- Replacing the Paperless-ngx database.
- Direct PDF editing with perfect invisible text layer placement.
- Full RAG/chat over all documents.
- Multi-tenant SaaS.
- Complex workflow designer UI.

These can be future extensions, but the MVP should focus on reliable document
processing and auditability.

## 4. MVP Scope

The MVP must process existing and new Paperless-ngx documents using trigger tags.

Required stages:

- `ocr`: run vision OCR and update the Paperless document content.
- `tags`: select up to N Paperless tags from existing tags.
- `title`: suggest or apply a better document title.
- `correspondent`: select an existing correspondent.
- `document_type`: select an existing document type.

Required modes:

- `review`: store suggestions and wait for approval.
- `autopilot`: apply suggestions immediately if validation passes.

Required integrations:

- Paperless-ngx REST API.
- Ollama chat/vision API.
- OpenAI API.
- Anthropic Claude API.
- OpenAI-compatible API endpoints.
- PostgreSQL 18-backed job state.
- authentication/session storage.
- audit logging.

Required deployment:

- one API/UI deployment
- one or more worker deployments
- one PostgreSQL 18 database
- Kubernetes secrets for Paperless and model provider credentials
- Docker Compose support for single-host deployments

## 5. High-Level Architecture

```text
              +-------------------+
              |   Paperless-ngx   |
              | documents + API   |
              +---------+---------+
                        ^
                        | REST API only
                        v
+-----------------------+-----------------------+
|                Paperless Archivist            |
|                                               |
|  +-------------+   +-----------------------+  |
|  | API Server  |   | Worker                |  |
|  | Axum        |   | Tokio jobs            |  |
|  +------+------+   +-----------+-----------+  |
|         |                      |              |
|         v                      v              |
|  +-------------+   +-----------------------+  |
|  | PostgreSQL  |   | AI Providers          |  |
|  | jobs/audit  |   | Ollama/OpenAI compat  |  |
|  +-------------+   +-----------------------+  |
|                                               |
+-----------------------------------------------+
```

## 6. Runtime Components

### 6.1 API/UI Server

Rust service using Axum.

Responsibilities:

- health checks
- readiness checks
- login/logout UI
- user/session management API
- role-based authorization
- static UI serving
- serving built frontend assets
- OpenAPI specification
- settings API
- settings UI
- prompt management API
- prompt editor UI
- review queue API
- review queue UI
- run history API
- backlog dashboard UI
- manual trigger API
- model test endpoint
- metrics endpoint

The API/UI server does not execute long-running AI work. It creates jobs, serves
state from PostgreSQL, exposes the OpenAPI contract, and serves the built React
frontend assets.

For the MVP, frontend and backend are separated in code and communicate through
`/api/*` endpoints. They can still be deployed together by serving the built
frontend from the Rust API container. Separate frontend deployment remains
possible later.

The UI must require authentication by default. Security details are documented in
[Security Design](SECURITY_DESIGN.md).

### 6.1.1 Frontend

React + TypeScript application.

Responsibilities:

- login and session UI
- backlog dashboard
- review queue
- prompt editor
- provider/model settings
- workflow tag settings
- user and role management
- audit log views
- batch processing controls
- run/job detail views

Recommended stack:

- React
- TypeScript
- Vite
- pnpm
- TanStack Query
- TanStack Table
- React Hook Form
- Zod
- generated OpenAPI client

Frontend validation improves usability only. Rust backend validation remains
authoritative.

### 6.2 Worker

Rust async worker using Tokio.

Responsibilities:

- poll Paperless-ngx for trigger tags
- create pipeline runs
- lease pending jobs from PostgreSQL
- call Paperless-ngx API
- render documents for OCR
- call AI providers
- validate outputs
- apply approved/autopilot changes
- set completion tags
- write audit events

Multiple workers must be safe. Use PostgreSQL row locks and per-document
idempotency to avoid two workers processing the same document at once.

### 6.3 PostgreSQL 18

PostgreSQL stores Archivist operational state only.

Do store:

- jobs
- pipeline runs
- prompt versions
- model/provider config
- AI artifacts
- review decisions
- audit events
- cached Paperless metadata

PostgreSQL 18 is mandatory. The detailed database design is documented in
[PostgreSQL 18 Design](POSTGRESQL_18_DESIGN.md).

Do not store:

- full document archive
- Paperless-ngx source of truth data as authoritative copies
- Paperless user passwords

### 6.4 AI Provider Layer

Provider abstraction with two capabilities:

- text/chat completion
- vision completion

Initial providers:

- Ollama
- OpenAI
- Anthropic Claude
- OpenAI-compatible HTTP API

The provider layer must expose a normalized request/response type. The rest of
the application must not depend on provider-specific JSON.

Provider configuration must allow separate models per stage:

- vision OCR
- OCR cleanup
- tagging
- title generation
- correspondent classification
- document type classification
- custom field extraction

## 7. Repository Layout

Recommended Rust workspace:

```text
paperless-archivist/
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
    src/
  migrations/
  docs/
  deploy/
    kubernetes/
    helm/
  tests/
    fixtures/
```

Crate responsibilities:

- `archivist-core`: domain types, stage definitions, validation, diffs.
- `archivist-db`: SQLx database access and migrations.
- `archivist-paperless`: Paperless-ngx API client.
- `archivist-ai`: AI provider clients and prompt execution.
- `archivist-ocr`: PDF/image extraction and OCR orchestration.
- `archivist-api`: Axum HTTP API.
- `archivist-worker`: worker binary.
- `archivist-config`: config loading and validation.
- `frontend`: React + TypeScript UI.

## 8. Paperless Integration Contract

Archivist must use the Paperless-ngx REST API only.

Required API operations:

- list documents
- fetch one document
- download original document
- list tags
- create tag if allowed
- patch document metadata
- patch document content
- list correspondents
- list document types
- list custom fields

The Paperless client must handle pagination, retries, HTTP timeouts, and API
errors with useful context.

### 8.1 Trigger Tags

Default trigger tags:

```text
ai-process
ai-ocr
ai-tags
ai-title
ai-correspondent
ai-document-type
ai-fields
```

Meaning:

- `ai-process`: run the default full pipeline.
- `ai-ocr`: run OCR only.
- `ai-tags`: run tag selection only.
- `ai-title`: run title generation only.
- `ai-correspondent`: run correspondent selection only.
- `ai-document-type`: run document type selection only.
- `ai-fields`: run custom field extraction only.

### 8.2 Completion Tags

Default completion tags:

```text
ai-processed
archivist-ocr
archivist-tags
ai-processed-title
ai-processed-correspondent
ai-processed-document-type
ai-processed-fields
```

Rules:

- Completion tags are set by Archivist code, not by AI prompts.
- Trigger tags are removed after the corresponding stage succeeds.
- If a stage fails, the trigger tag remains unless policy says otherwise.
- Completion tags are never presented to the tag-selection prompt as normal
  business tags.

## 9. Pipeline Model

A pipeline run belongs to one Paperless document and contains one or more stages.

```text
queued -> running -> waiting_review -> applying -> succeeded
                         |              |
                         v              v
                       rejected        failed
```

### 9.1 Full Pipeline

Default `ai-process` order:

1. `ocr`
2. `ocr_fix`
3. `title`
4. `document_type`
5. `correspondent`
6. `tags`
7. `fields`
8. `apply`

The pipeline should be configurable, but the MVP can hard-code the default order.

### 9.2 OCR Stage

Input:

- Paperless document ID
- original file bytes
- OCR page limit
- language hint
- existing Paperless content

Process:

1. Download original file from Paperless.
2. Render first N pages to images.
3. Send page images to vision model.
4. Normalize page outputs into one text.
5. Validate minimum confidence heuristics.
6. Store OCR artifact.
7. In `full_auto`, patch Paperless document `content`.
8. Set `archivist-ocr`.

MVP rendering strategy:

- Use external tools inside the container, e.g. Poppler or MuPDF.
- Wrap the command execution in Rust and isolate temp files.
- Later replace with a native Rust renderer if it becomes stable enough.

### 9.3 Metadata Stages

Each metadata stage uses explicit allowed choices from Paperless.

Examples:

- tags: allowed tag names, max 5, no workflow tags
- correspondent: existing correspondent names only
- document type: existing document type names only
- fields: fields allowed for the detected/current document type

The model should return structured JSON where possible. For tag selection, the
internal normalized output should be:

```json
{
  "tags": ["Steuern", "Versicherung"],
  "new_tags": [],
  "confidence": 0.82
}
```

Prompt text can request a compact format, but code must normalize to typed
domain objects before applying anything.

## 10. Autopilot and Review

### 10.1 Review Mode

Default safe mode.

Flow:

1. Worker generates suggestions.
2. Suggestions are stored in PostgreSQL.
3. API exposes pending review.
4. User approves, edits, or rejects.
5. Apply job writes approved changes to Paperless.
6. Audit event records the decision.

### 10.2 Workflow Modes

Archivist supports manual review, automatic document selection with review, and
full autopilot. Full autopilot applies changes without human approval only if
all validation rules pass.

Recommended MVP validation:

- only existing tags unless `allow_new_tags=true`
- max tag count enforced
- no workflow tags as business tags
- correspondent must be known
- document type must be known
- title must be below configured max length
- OCR output must be non-empty
- model response must parse cleanly

If validation fails, the run falls back to review or failed state based on policy.

## 11. PostgreSQL Schema Draft

### Auth and Security Tables

Security tables are part of the MVP schema.

`users`:

- `id uuid primary key default uuidv7()`
- `username text not null unique`
- `email text unique`
- `password_hash text not null`
- `enabled boolean not null default true`
- `last_login_at timestamptz`
- `failed_login_count integer not null default 0`
- `password_changed_at timestamptz`
- `created_at timestamptz not null`
- `updated_at timestamptz not null`

`user_roles`:

- `user_id uuid not null references users(id)`
- `role text not null`
- primary key over `(user_id, role)`

`sessions`:

- `id uuid primary key default uuidv7()`
- `user_id uuid not null references users(id)`
- `session_hash text not null unique`
- `csrf_secret_hash text not null`
- `expires_at timestamptz not null`
- `revoked_at timestamptz`
- `last_seen_at timestamptz`
- `created_at timestamptz not null`

`api_tokens`:

- `id uuid primary key default uuidv7()`
- `name text not null`
- `token_hash text not null unique`
- `scopes text[] not null`
- `created_by uuid references users(id)`
- `expires_at timestamptz`
- `revoked_at timestamptz`
- `last_used_at timestamptz`
- `created_at timestamptz not null`

`secret_references`:

- `id uuid primary key default uuidv7()`
- `name text not null unique`
- `kind text not null` (`kubernetes_secret`, `docker_secret`, `env`,
  `mounted_file`, `encrypted_value`)
- `reference jsonb not null`
- `created_by uuid references users(id)`
- `updated_by uuid references users(id)`
- `created_at timestamptz not null`
- `updated_at timestamptz not null`

Plain provider secrets should not be stored in normal settings rows.

### 11.1 `settings`

Stores non-secret runtime settings.

Columns:

- `key text primary key`
- `value jsonb not null`
- `updated_at timestamptz not null`

Secrets should come from Kubernetes secrets or environment variables, not this
table.

### 11.2 `ai_providers`

Columns:

- `id uuid primary key`
- `name text not null unique`
- `kind text not null` (`ollama`, `openai_compatible`)
- `base_url text not null`
- `default_text_model text`
- `default_vision_model text`
- `enabled boolean not null default true`
- `created_at timestamptz not null`
- `updated_at timestamptz not null`

### 11.3 `prompts`

Columns:

- `id uuid primary key`
- `stage text not null`
- `name text not null`
- `version integer not null`
- `content text not null`
- `output_schema jsonb`
- `active boolean not null`
- `created_at timestamptz not null`

Constraint:

- only one active prompt per stage/name pair.

### 11.4 `pipeline_runs`

Columns:

- `id uuid primary key`
- `paperless_document_id integer not null`
- `mode text not null` (`review`, `autopilot`)
- `trigger_tag text not null`
- `status text not null`
- `stages jsonb not null`
- `started_at timestamptz`
- `finished_at timestamptz`
- `error_message text`
- `created_at timestamptz not null`
- `updated_at timestamptz not null`

Indexes:

- `(paperless_document_id, status)`
- `(status, created_at)`

### 11.5 `jobs`

Columns:

- `id uuid primary key`
- `run_id uuid not null references pipeline_runs(id)`
- `paperless_document_id integer not null`
- `stage text not null`
- `status text not null`
- `attempts integer not null default 0`
- `max_attempts integer not null default 3`
- `lease_owner text`
- `lease_until timestamptz`
- `payload jsonb not null`
- `result jsonb`
- `error_message text`
- `created_at timestamptz not null`
- `run_after timestamptz not null`
- `updated_at timestamptz not null`

Indexes:

- `(status, run_after)`
- `(lease_until)`
- `(paperless_document_id, stage, status)`

Workers claim jobs using `FOR UPDATE SKIP LOCKED`.

### 11.6 `ai_artifacts`

Columns:

- `id uuid primary key`
- `run_id uuid not null references pipeline_runs(id)`
- `job_id uuid references jobs(id)`
- `stage text not null`
- `provider text not null`
- `model text not null`
- `prompt_id uuid references prompts(id)`
- `input_hash text not null`
- `request jsonb`
- `response jsonb`
- `normalized_output jsonb`
- `duration_ms integer`
- `created_at timestamptz not null`

### 11.7 `review_items`

Columns:

- `id uuid primary key`
- `run_id uuid not null references pipeline_runs(id)`
- `paperless_document_id integer not null`
- `status text not null` (`pending`, `approved`, `rejected`, `edited`)
- `suggested_patch jsonb not null`
- `edited_patch jsonb`
- `reviewed_by text`
- `reviewed_at timestamptz`
- `created_at timestamptz not null`

### 11.8 `audit_events`

Columns:

- `id uuid primary key`
- `run_id uuid references pipeline_runs(id)`
- `paperless_document_id integer`
- `event_type text not null`
- `actor text not null`
- `before jsonb`
- `after jsonb`
- `metadata jsonb`
- `created_at timestamptz not null`

## 12. Idempotency and Concurrency

Rules:

- One active run per Paperless document unless explicitly forced.
- One worker may own a job lease at a time.
- Completion tags prevent accidental repeat processing.
- Re-running a stage with `force=true` creates a new run and records why.
- Applying changes must re-fetch the current Paperless document first.
- If Paperless metadata changed since suggestion generation, detect conflicts and
  send the item back to review.

## 13. API Design Draft

### Health and Metrics

```text
GET /healthz
GET /readyz
GET /metrics
```

### Authentication

```text
POST /api/auth/login
POST /api/auth/logout
GET  /api/auth/me
POST /api/auth/change-password
POST /api/auth/sessions/{id}/revoke
```

### Users and Roles

```text
GET  /api/users
POST /api/users
GET  /api/users/{id}
PUT  /api/users/{id}
POST /api/users/{id}/disable
POST /api/users/{id}/enable
PUT  /api/users/{id}/roles
```

### API Tokens

```text
GET    /api/api-tokens
POST   /api/api-tokens
DELETE /api/api-tokens/{id}
```

### Secret References

```text
GET  /api/secret-references
POST /api/secret-references
PUT  /api/secret-references/{id}
POST /api/secret-references/{id}/test
```

### Settings

```text
GET  /api/settings
PUT  /api/settings
POST /api/model-providers/test
```

### Prompts

```text
GET  /api/prompts
POST /api/prompts
GET  /api/prompts/{id}
PUT  /api/prompts/{id}
POST /api/prompts/{id}/activate
```

### Runs and Jobs

```text
GET  /api/runs
GET  /api/runs/{id}
POST /api/documents/{paperless_document_id}/trigger
POST /api/runs/{id}/retry
POST /api/runs/{id}/cancel
```

### Review

```text
GET  /api/reviews
GET  /api/reviews/{id}
POST /api/reviews/{id}/approve
POST /api/reviews/{id}/reject
POST /api/reviews/{id}/edit
```

### Paperless Metadata Cache

```text
POST /api/paperless/sync-metadata
GET  /api/paperless/tags
GET  /api/paperless/correspondents
GET  /api/paperless/document-types
GET  /api/paperless/custom-fields
```

## 14. Configuration

Users must be able to configure normal runtime behavior from the UI. Editing
Kubernetes manifests, Docker Compose files, or environment variables must not be
required for day-to-day model, prompt, workflow, or batch-processing changes.

UI-managed settings:

- Paperless base URL
- Paperless public URL
- AI providers
- text model
- vision model
- per-stage model overrides
- workflow trigger tag names
- workflow completion tag names
- prompt versions
- enabled stages
- review/autopilot mode
- OCR page limits
- tag behavior
- retry policy
- batch processing rules
- dashboard preferences

Bootstrap environment variables:

```text
ARCHIVIST_HTTP_ADDR=0.0.0.0:8080
ARCHIVIST_WORKER_CONCURRENCY=2
DATABASE_URL=postgres://...
ARCHIVIST_BOOTSTRAP_ADMIN_TOKEN=...
ARCHIVIST_CONFIG_IMPORT=/config/bootstrap.yaml
ARCHIVIST_MODE=review
ARCHIVIST_LOG_LEVEL=info
```

Runtime settings in DB:

- default mode
- OCR page limit
- max tags
- allow new tags
- enabled stages
- retry policy
- workflow tag names
- model per stage

Secrets:

- Kubernetes: referenced from Kubernetes secrets.
- Docker Compose: referenced from Docker secrets, mounted files, or environment
  variables.
- UI stores secret references and metadata, not plain secret values in normal
  settings rows.

## 15. Prompt Strategy

Prompts are versioned. A run must always reference the exact prompt version that
produced an output.

General prompt rules:

- Give the model the allowed vocabulary.
- Demand strict output.
- Prefer JSON for machine-applied stages.
- Reject unknown values by code, not by trust in the prompt.
- Keep workflow tags out of available business tag lists.

Tag prompt policy:

- Default: choose existing tags only.
- Max tags: 5.
- New tags require `allow_new_tags=true`.
- New tags must be reusable, not OCR noise.
- Completion tags are never suggested by AI.

## 16. Security

Security is a first-class product requirement, not a deployment afterthought.

Required in the first usable release:

- user login
- local users with Argon2id password hashing
- role-based access control
- server-side sessions
- CSRF protection
- scoped API tokens
- audit logs
- secure secret handling
- no unauthenticated admin UI
- document text not logged by default

Required deployment hardening:

- store API tokens in Kubernetes secrets
- never log full document text by default
- redact Authorization headers
- support NetworkPolicies in Kubernetes
- run containers as non-root
- read-only root filesystem if practical
- use temporary storage for rendered pages
- delete temp files after each job
- make external AI providers opt-in

Authentication options:

MVP:

- local users in PostgreSQL 18
- admin bootstrap flow
- secure server-side sessions

Later:

- authenticate against Paperless-ngx and map Paperless users to review actions.
- OIDC SSO
- external secret manager integration

Detailed design:

- [Security Design](SECURITY_DESIGN.md)

## 17. Observability

Logs:

- structured JSON logs
- include run ID, job ID, document ID, stage, attempt
- never include full document content unless debug mode explicitly allows it

Metrics:

- jobs queued/running/failed/succeeded
- stage duration
- AI provider latency
- Paperless API latency
- retry count
- review queue length
- OCR pages processed

Tracing:

- one trace per pipeline run
- spans per Paperless API call and AI call

## 18. Kubernetes Deployment

MVP resources:

```text
Namespace: paperless-archivist

Deployment:
  paperless-archivist-api

Deployment:
  paperless-archivist-worker

Service:
  paperless-archivist-api

Ingress:
  archivist.example.com

Secret:
  paperless-archivist-runtime

PostgreSQL:
  central PostgreSQL 18 database or dedicated cluster
```

Worker requirements:

- temp storage for PDF page rendering
- network access to Paperless API
- network access to Ollama
- network access to PostgreSQL

GPU is not required by Archivist itself if Ollama runs separately.

## 19. Docker Compose Deployment

Docker Compose is a supported deployment mode for evaluation, local development,
and small single-host installs.

MVP services:

```text
paperless-archivist-api
paperless-archivist-worker
postgres
ollama optional
```

Compose requirements:

- PostgreSQL 18 image
- API/UI and worker can use the same application image
- worker and API share the same `DATABASE_URL`
- configuration is completed in the UI after bootstrap
- optional Ollama service can be enabled locally
- external Paperless-ngx URL can point to an existing Paperless deployment
- named volumes for PostgreSQL and optional cache/temp data

Application code must not assume Kubernetes APIs are available. Kubernetes
integration is deployment-layer behavior, not core application behavior.

## 20. Failure Handling

Classify failures:

- transient Paperless API error: retry
- transient AI provider error: retry
- model parse error: retry with repair prompt once, then review/failed
- validation error: review
- Paperless conflict: review
- permission error: failed
- missing source file: failed

Retries:

- exponential backoff
- max attempts per stage
- dead-letter failed jobs after max attempts
- manual retry endpoint

## 21. Testing Strategy

Unit tests:

- prompt output parsers
- validation rules
- Paperless metadata mapping
- patch/diff generation
- job state transitions

Integration tests:

- mock Paperless API
- mock Ollama/OpenAI-compatible API
- PostgreSQL test database
- worker claims jobs with `SKIP LOCKED`
- idempotent reruns

Golden tests:

- sample German invoices
- sample insurance letters
- sample pharmacy receipts
- sample tax documents
- expected tag/title/type outputs

End-to-end tests:

- start Paperless fixture or mocked API
- queue document with `ai-process`
- worker creates run
- suggestions are generated
- review approves
- Paperless patch is sent
- completion tags are set

## 22. Implementation Milestones

### Milestone 0: Project Foundation

- Rust workspace
- React/Vite/pnpm frontend skeleton
- CI pipeline
- formatting and linting
- Dockerfile
- Kubernetes skeleton
- Docker Compose skeleton
- configuration loading
- health endpoint

### Milestone 1: Security and DB Foundation

- SQLx migrations
- local users
- sessions
- RBAC
- scoped API tokens
- audit event writer
- jobs table
- pipeline run table
- worker job leasing

### Milestone 2: API Contract, UI Shell, and Paperless Inventory

- OpenAPI contract
- generated TypeScript client
- React app shell
- login UI
- Paperless client
- metadata sync
- document inventory
- backlog dashboard

### Milestone 3: Tagging MVP

- prompt storage
- Ollama text provider
- tag suggestion
- validation
- review item creation
- autopilot apply
- completion tags

### Milestone 4: OCR MVP

- document download
- PDF page rendering
- Ollama vision provider
- OCR output normalization
- content update
- OCR completion tag

### Milestone 5: Review UI and Batch Processing

- review list
- diff view
- approve/reject/edit
- run history
- prompt editor
- queue all missing OCR
- queue all missing tagging
- retry failed subset

### Milestone 6: Full Metadata Pipeline

- title
- correspondent
- document type
- custom fields
- conflict detection

### Milestone 7: Production Hardening

- metrics
- tracing
- network policies
- backup/restore notes
- load tests
- security review

## 23. MVP Acceptance Criteria

The MVP is done when:

- UI requires login.
- Local admin bootstrap is documented.
- RBAC exists for admin/operator/reviewer/viewer.
- A document tagged `ai-process` is detected automatically.
- Archivist creates a pipeline run and jobs in PostgreSQL.
- Worker processes OCR and tag selection using Ollama.
- Output is validated before applying.
- In review mode, suggestions wait for approval.
- In autopilot mode, valid suggestions are applied.
- Trigger tags are removed after success.
- Completion tags are added.
- Every applied change has an audit event.
- Settings changes have audit events.
- Provider secret changes have audit events.
- Failed jobs are visible and retryable.
- The service can restart without losing or duplicating work.
- Normal runtime configuration is possible from the UI.
- Kubernetes deployment manifests exist.
- Docker Compose deployment exists.

## 24. Decided MVP Architecture

These decisions are no longer open for MVP implementation:

- React + TypeScript web UI is part of the MVP.
- Rust/Axum API and Rust/Tokio worker are the backend runtime.
- PostgreSQL 18 is mandatory.
- PostgreSQL is the job queue; Redis is not required.
- Local users, login, RBAC, sessions, CSRF, scoped API tokens, and audit logs are
  required.
- UI-managed runtime configuration is required.
- Existing tags only are allowed in autopilot.
- New business tags require review by default.
- OCR updates Paperless document `content` first.
- PDF text-layer replacement is a later feature.
- Ollama is the first local AI provider.
- OpenAI, Anthropic Claude, and OpenAI-compatible APIs are supported provider
  targets.
- Kubernetes is the primary production target.
- Docker Compose is a supported deployment target.
- A dedicated PostgreSQL database and role are required. The database may live on
  an existing PostgreSQL 18 cluster or on a Compose-managed PostgreSQL 18
  service.

## 25. Remaining Non-Blocking Decisions

These choices should be resolved before a public release, but they do not block
MVP implementation:

1. OSS license.
2. UI component/styling library, if plain CSS becomes insufficient.
3. Whether Paperless-ngx login bridge ships before or after OIDC.
4. Whether generated OpenAPI comes from Rust route metadata or a checked-in
   OpenAPI specification.
5. Whether the first Kubernetes deployment uses Helm, raw manifests, or
   Kustomize as the canonical package.
