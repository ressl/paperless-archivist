# Architecture Decisions

Status: draft

This document records explicit architecture decisions so implementation does not
drift.

## ADR-001: Rust Backend/Core With React Frontend

Decision: Paperless Archivist uses Rust for all backend, worker, domain, and
integration logic. The frontend is implemented as a React + TypeScript
application.

Scope:

- API server: Rust
- worker: Rust
- Paperless client: Rust
- AI provider clients: Rust
- OCR orchestration: Rust
- database access: Rust
- CLI: Rust
- frontend: React + TypeScript
- frontend build tooling: Node.js + pnpm

Rationale:

- one language for operational/backend code
- strong typing around AI output validation
- good async runtime with Tokio
- low memory footprint for Kubernetes
- simple static binaries
- good PostgreSQL support via SQLx
- reliable background worker implementation
- better UI quality for complex review, audit, backlog, prompt, and settings
  workflows
- easier implementation of rich tables, filters, diffs, forms, and future chat
  interfaces

Allowed exceptions:

- external command-line tools for PDF rendering/OCR helpers, e.g. Poppler or
  MuPDF, called by Rust
- generated OpenAPI clients and frontend build artifacts
- static frontend assets served by the Rust API or a separate static server

Not allowed for MVP:

- Node.js backend
- Python worker
- business logic implemented only in the frontend
- unaudited direct frontend access to Paperless or AI providers

## ADR-002: Frontend and Backend Are Separated by Code and API Boundary

Decision: Keep frontend and backend logically separated, but ship them as one
deployable service for the MVP unless an operator chooses separate deployments.

Implementation:

- Backend API is Rust/Axum.
- Frontend is React + TypeScript.
- Frontend calls `/api/*` endpoints only.
- OpenAPI is the contract between frontend and backend.
- Static frontend assets are shipped in the API container for the MVP.
- Worker remains a separate Rust binary/deployment.

Rationale:

- clear API boundary
- rich UI is easier with React
- API remains usable for CLI and automation
- one-container deployment stays simple
- separate frontend deployment remains possible later

Recommended MVP UI stack:

- React
- TypeScript
- Vite
- pnpm
- TanStack Query for API state
- TanStack Table for backlog/audit tables
- React Hook Form plus Zod for forms and validation
- generated OpenAPI client

The frontend must not contain trusted business logic. It can validate forms for
user experience, but backend Rust validation is authoritative.

## ADR-003: PostgreSQL 18 Is Mandatory

Decision: PostgreSQL 18 is the only supported database.

Rationale:

- workflow queue
- audit log
- document inventory
- reporting
- prompt history
- future pgvector support
- PostgreSQL 18 features such as `uuidv7()`, async I/O, generated columns, and
  `OLD`/`NEW` in `RETURNING`

No SQLite fallback will be implemented.

## ADR-004: AI Provider Abstraction Supports Local and API LLMs

Decision: AI access is abstracted behind provider traits.

Required providers:

- Ollama for local LLMs
- OpenAI API
- Anthropic Claude API
- OpenAI-compatible API endpoints

Capabilities:

- text/chat completion
- vision completion
- structured output if provider supports it
- streaming later, not required for MVP

Rationale:

- local-first deployment with Ollama
- optional high-quality external models for users who want them
- provider-specific behavior stays isolated
- prompts and validation stay provider-independent

Provider configuration must support separate models per stage:

- OCR vision model
- OCR cleanup model
- tagging model
- title model
- correspondent model
- document type model
- fields extraction model

Example:

```text
vision_ocr      -> ollama/qwen2.5vl:7b
ocr_fix         -> ollama/qwen3:8b
tags            -> openai/gpt-5.5
fields          -> anthropic/claude-sonnet-4-6
high_volume_ocr -> ollama-cloud/qwen3-vl:235b-instruct
```

## ADR-005: AI Output Is Always Validated by Rust Types

Decision: AI output is never applied directly.

Every provider response must be normalized into Rust domain types:

- `OcrText`
- `TagSuggestion`
- `TitleSuggestion`
- `CorrespondentSuggestion`
- `DocumentTypeSuggestion`
- `FieldSuggestion`
- `DocumentPatch`

Then validation runs before review or apply.

Rationale:

- prompts are not security boundaries
- models can hallucinate
- different providers return different shapes
- Rust type validation is the real contract

## ADR-006: Paperless-ngx Remains the System of Record

Decision: Archivist uses only the Paperless-ngx API for Paperless data changes.

No direct writes to the Paperless database.

Rationale:

- avoids coupling to Paperless internals
- preserves Paperless permissions and side effects
- keeps upgrade path clean
- makes Archivist deployable beside any Paperless instance

## ADR-007: Runtime Configuration Is Managed in the UI

Decision: Users must be able to configure the product from the web UI.

The UI must support configuration for:

- Paperless connection
- AI providers
- text and vision models
- per-stage model selection
- prompts
- workflow tag names
- OCR page limits
- stage enable/disable
- review/autopilot mode
- batch processing rules
- tag behavior
- retry behavior
- dashboard/reporting preferences

Environment variables are only for bootstrap and secrets:

- HTTP bind address
- database URL
- initial admin/bootstrap settings
- secret references
- optional config import/export path

Rationale:

- Paperless-AIssist shows that UI-managed configuration is easier for users.
- Users should not edit YAML or restart pods for normal prompt/model changes.
- Kubernetes and Docker Compose deployments should behave the same after
  bootstrap.
- Prompt and model experiments need versioning and auditability.

Implementation:

- non-secret settings are stored in PostgreSQL 18
- secrets are referenced from Kubernetes secrets, Docker secrets, mounted files,
  or environment variables
- the UI stores only secret references or encrypted secret records, never plain
  API keys in normal settings tables
- all settings changes produce audit events

## ADR-008: Kubernetes-First, Docker-Compose-Friendly

Decision: Kubernetes is the primary production target, but Docker Compose is a
supported deployment mode.

Kubernetes requirements:

- stateless API/UI deployment
- stateless worker deployment
- PostgreSQL 18 database
- Kubernetes secrets
- health/readiness probes
- metrics endpoint
- graceful shutdown
- horizontal worker scaling
- network policy friendly
- rootless container
- read-only root filesystem where practical

Docker Compose requirements:

- one API/UI service
- one worker service
- one PostgreSQL 18 service
- optional Ollama service
- named volumes for database and optional local cache
- `.env` bootstrap file
- no Kubernetes-only assumptions in application code

Rationale:

- the project should work well in homelabs and small teams
- Kubernetes should not be required for evaluation
- Docker Compose is useful for local development and simple installs
- runtime configuration in UI keeps both deployment types consistent

## ADR-009: Login and Enterprise Security Are Required

Decision: Paperless Archivist must include authentication, authorization, audit
logging, and secure defaults from the first usable release.

Required:

- user login
- local users with Argon2id password hashing
- role-based access control
- server-side sessions
- CSRF protection
- scoped API tokens
- audit logging
- secret redaction
- secure-by-default UI

Enterprise-ready design targets:

- OIDC SSO
- Paperless-ngx login bridge
- custom roles later
- SIEM-friendly logs
- metrics and traces
- external secret references
- hardened production deployment manifests outside the public source tree

Rationale:

- documents are sensitive
- AI provider configuration can leak private data if misused
- batch processing can change thousands of documents
- review/apply decisions need accountability
- enterprise users need SSO, audit, and least-privilege operation

Detailed design is documented in [Security Design](SECURITY_DESIGN.md).

## ADR-010: Dashboard Snapshot Bucketing

Decision: The `dashboard_snapshots` table is written by the worker tick loop
(not by the `/dashboard` read path) and dedupes inserts within a 5-minute
existence guard, while the dashboard backlog series is rendered at an
*hourly* (or coarser) granularity by querying the same table.

Rationale:

- Coalescing writes to one row every five minutes keeps the table linear in
  time rather than linear in concurrent dashboard polls. With dashboards
  refreshing every 30 seconds, the previous read-path-writes scheme could
  produce hundreds of identical rows per hour per polling browser.
- The worker is the single writer (see #97), so there is no read/write
  contention on the table during normal operation; the dashboard endpoint
  only reads.
- Five minutes is short enough that the "live" KPIs remain meaningful (the
  same backlog values are surfaced via `/dashboard/live`, which queries the
  source-of-truth `document_inventory` directly and is not bucketed), and
  coarse enough to absorb burst worker activity into one snapshot row.
- The dashboard backlog chart aggregates at hourly granularity for ranges
  longer than 24h, so the 5-minute write cadence still produces 12 candidate
  rows per chart bucket. The `select ... order by captured_at desc limit 1`
  lateral join in `backlog_series` deliberately picks the most recent
  snapshot inside each bucket — this trades visual continuity for storage
  cost and is the documented trade-off.

Implications:

- Backfilling historical snapshots requires a worker run, not a dashboard
  view. Tests that need historical buckets seed `dashboard_snapshots`
  directly.
- A worker outage longer than five minutes will produce gaps in the
  backlog chart. The empty-state fallback in `backlog_series` synthesises
  a single "now" row from live counts so the chart still renders.

## ADR-011: Consolidated Metadata Stage

Decision: Replace the six per-field metadata stages (`Title`, `DocumentType`,
`Correspondent`, `DocumentDate`, `Tags`, `Fields`) with one consolidated
`Stage::Metadata` that issues a single structured-JSON LLM call and yields up
to six review items (one per populated field). The default selector sequence
becomes `[Ocr, Metadata]`. Legacy per-field stages stay in the enum so
in-flight runs queued before v1.4.0 keep draining.

Rationale:

- Six independent LLM round-trips on the same document text cost six system
  prompts, six context windows, and six request-response RTTs. A single
  structured call drops total token spend ~5x and wall-clock latency ~6x in
  practice. The closed-vocabulary allowlists (correspondents, document
  types, tags, custom-field names) are only embedded once.
- The consolidated prompt only requests fields whose flag is true in
  `MetadataFieldFlags::from_enabled_stages(enabled_stages)`, so operators
  who keep per-field opt-outs do not pay for fields they disabled.
- Per-field validation contracts stay byte-for-byte identical — the worker
  delegates each subfield to the existing `validate_*` helpers
  (`validate_title_suggestion`, `validate_choice_suggestion`,
  `validate_document_date_suggestion`, `validate_tag_suggestion`,
  `validate_field_suggestion`). Closed-vocabulary correctness is not
  weakened by the consolidation.
- The fan-out shape (one review item per field with a `field` discriminator
  inside `suggested_patch.standard_metadata`) keeps the existing reviewer
  UX working: items still render per-field, can be approved/rejected
  individually, and the full_auto path can either auto-apply a single
  composite Paperless patch or fall back to per-field review.

Implications:

- `Stage::all_business_stages()` returns `[Ocr, Metadata]` for new runs;
  callers that still need the per-field enum variants use
  `Stage::legacy_per_field_stages()`.
- `document_inventory.metadata_status` is the column for the consolidated
  stage; `missing_pipeline_stages_for_inventory` consults both that column
  AND the legacy per-field columns so v1.3 inventory rows still flow
  through the v1.4 selector without a backfill migration.
- Prompt management UI still exposes the six legacy stage prompts; their
  help copy is marked deprecated and operators are directed to the
  consolidated `metadata` prompt for new tuning.
- Per-field overwrite guards (`metadata.overwrite_existing_correspondent`,
  `metadata.overwrite_existing_document_type`,
  `metadata.overwrite_existing_document_date`) continue to apply inside
  the consolidated handler. Each field can independently fall back to
  review or skip.

## ADR-012: Age-Derived Job Priority With Manual Override

Decision: Job rows carry two priority columns derived from `payload`:

- `priority`        — cross-run ordering (smaller wins). Manual triggers
                       stamp `0`; auto-selected runs stamp
                       `1_000_000 - paperless_document_id`.
- `stage_priority`  — within-run stage ordering (smaller wins). Stage 1
                       gets 10, stage 2 gets 20, etc.

`claim_jobs` orders by `priority, stage_priority, run_after, created_at`
and uses `stage_priority` (not `priority`) in the within-run dependency
subquery so a single key does not have to serve two semantic roles.

Rationale:

- Operators expect a fresh scan or a manual "re-queue" to show up in the
  UI in seconds, not after the auto-selector drains its backlog. A single
  priority value with the age formula lets the queue self-order without a
  separate "high priority" lane and without operator config.
- Splitting cross-run priority from within-run stage priority cleanly
  preserves the historical `not exists prev.priority < jobs.priority`
  subquery contract. Without the split, jobs of the same run would share
  one priority and the subquery would no longer enforce stage ordering.
- Saturating arithmetic in `age_derived_priority(doc_id)` keeps the result
  in `[1, 1_000_000]` so even synthetic doc ids beyond a million never
  drop below the manual-trigger floor of `0`.

Implications:

- A pre-existing v1.3 job (no `stage_priority` key in payload) inherits its
  stage ordering from the legacy `payload->>'priority'` value via the
  migration's `coalesce` fallback. The split is fully backward compatible.
- A future reschedule API can change a job's cross-run priority by editing
  `payload->>'priority'` without disturbing stage ordering.
- The `claim_jobs` retry bias (`order by case when error_message is not
  null ... then 0 else 1 end`) still runs first, so a stuck retry never
  starves out behind a flood of priority-0 manual triggers.
