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
- `stage_priority`  — stored generated column for within-run stage ordering
                       (smaller wins). Stage 1 gets 10, stage 2 gets 20,
                       etc.

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
- `stage_priority` is stored rather than virtual because PostgreSQL 18 does
  not support indexes on virtual generated columns.
- The `claim_jobs` retry bias (`order by case when error_message is not
  null ... then 0 else 1 end`) still runs first, so a stuck retry never
  starves out behind a flood of priority-0 manual triggers.

## ADR-013: Pin Ollama `options.num_ctx` Explicitly Instead of Relying on the Built-In Default

Context: Vision-OCR jobs against the v1.5.0 default vision model
(`glm-ocr:latest`) crashed Ollama's llama runner with
`GGML_ASSERT(a->ne[2] * 4 == b->ne[0])` on realistic single-page renders.
Upstream tracked the same assertion in
[ollama/ollama#14401](https://github.com/ollama/ollama/issues/14401) and
[ollama/ollama#14171](https://github.com/ollama/ollama/issues/14171) and
established that the assertion fires whenever the vision-token count for
the page exceeds the Ollama context window. Ollama's built-in default is
4096 tokens, which is too small for the vision-token expansion produced by
a normal letter-size page.

v1.5.0 shipped a fallback safety net — detect the crash signature, swap to
a different model, retry. That keeps documents flowing under Full-Auto but
masks the actual problem.

Decision: The worker explicitly sets `options.num_ctx` on every Ollama
vision and text payload, sourced from two new runtime settings:

- `RuntimeSettings.ai.ollama_vision_num_ctx` — default `16384`. Sized so a
  realistic multi-page render at high DPI still fits without bumping the
  ceiling.
- `RuntimeSettings.ai.ollama_text_num_ctx`   — default `8192`. Sized for
  the consolidated metadata prompt that embeds up to 16k chars of document
  content with prompt scaffolding.

Both fields are wired through `ChatRequest.num_ctx` /
`VisionRequest.num_ctx` as `Option<i64>` and only surface on the wire for
the Ollama provider. Remote providers (OpenAI / Anthropic /
OpenAI-compatible) ignore the field. The override is exposed in the
Settings UI under AI Defaults so operators can re-tune for unusual
hardware without redeploying.

Rationale:

- **Root-cause fix, not workaround.** Upstream identified the assertion as
  a context-window mismatch; setting `num_ctx` removes the precondition for
  the crash rather than catching the crash and retrying on another model.
- **Operator-tunable without code changes.** The default works on
  commodity Ollama hosts. Tiny boxes (low RAM) can lower the value;
  high-DPI multi-page scanners can raise it. The setting lives next to the
  other AI defaults so it surfaces during normal operations review.
- **Symmetric for text and vision.** The metadata prompt is also large
  (16k chars), so keeping text at the 4096 default would leave a smaller
  but related class of edge-case truncation/quality issues unaddressed.
  The text default is conservative (8k) because text tokens are cheaper
  than vision tokens.
- **Defense in depth stays.** The v1.5.0 fallback machinery is left in
  place. After v1.5.1 deploys, the expected steady state is that
  `vision_model_fallback_used=true` counts drop toward zero — but if some
  unforeseen page still trips the assertion, the fallback still catches it
  before the run dies.

Implications:

- New `RuntimeSettings.ai` fields carry `#[serde(default = "...")]` so
  existing rows in the `runtime_settings` table deserialize cleanly
  without a migration.
- `ChatRequest` and `VisionRequest` gain `num_ctx: Option<i64>` with
  `#[serde(default)]`. Any external caller constructing these structs
  manually needs `num_ctx: None` (compiler-enforced in-tree).
- The Ollama HTTP payload is built via two free functions
  (`build_ollama_chat_payload`, `build_ollama_vision_payload`) so the
  num_ctx wiring is unit-testable without spinning up an HTTP server.
- The startup helper `run_startup_vision_crash_requeue` (introduced in
  v1.5.0) is idempotent — it clears `error_message` on the rows it
  matches, so subsequent worker restarts cannot re-requeue the same job.
  Combined with the v1.5.1 num_ctx fix, the previously-dead OCR jobs
  succeed on first retry without going through the fallback path.

## ADR-014: SGLang MiniMax M3 Is a Text-First OpenAI-Compatible Provider

Status: Accepted on 2026-07-17 by issue #366.

### Context and pinned evidence

MiniMax M3 is a native multimodal, reasoning-capable MoE model. Native
multimodality does not by itself decide whether Archivist should replace its
OCR path. Mixing that product decision with protocol support would make the
provider, OCR, and release contracts depend on different assumptions.

The integration target is exactly
[`ressl/MiniMax-M3-uncensored-NVFP4`](https://huggingface.co/ressl/MiniMax-M3-uncensored-NVFP4),
not a product-name substring or an arbitrary model from another namespace. The
reference checkpoint revision reviewed for this decision is
`6863c5c62a892e2d1e886a69e134b3b866e0963e`. Its `chat_template.jinja` has
SHA-256 `11421244f67553498e5c8112dae02802025bcc4305ec45ad380af95c96f9fe64`,
which is byte-identical to the official
[MiniMax M3 template at revision `5094273`](https://huggingface.co/MiniMaxAI/MiniMax-M3/blob/50942730318c7943fe83db7ec8e9f9177ecb1cf8/chat_template.jinja).

The reviewed runtime is SGLang `0.0.0.dev1+g56e290315`, pinned by container
digest
`lmsysorg/sglang@sha256:8cc6e6f90bf803e9817800b679173d0b526f2b42b2c61b7ecafecdadb610eb55`.
MiniMax M3 support is newer than the latest tagged SGLang release at decision
time, so a mutable `latest` or development tag is not an acceptable production
pin. The model revision, image digest, and runtime revision must be emitted by
the opt-in live contract report in #371.

A read-only smoke against that exact model/runtime pair accepted all three
template values. `disabled` returned final content without
`reasoning_content`; `adaptive` and `enabled` returned the same final content
with a separately parsed reasoning trace, and none leaked `<mm:think>` into
`message.content`.

The base architecture advertises a 1,048,576-token context. That number is not
an Archivist capacity promise. Supported context, timeout, and concurrency are
the values proven on the target deployment by #373.

### Decision

#### Protocol and identity

SGLang remains `kind = openai_compatible`. Provider kinds describe wire
protocols, not server products. The M3-specific behavior is selected by an
explicit capability for the exact target model identity; broad checks such as
`contains("MiniMax-M3")` are not allowed. Other OpenAI-compatible models must
not receive M3-only request fields.

The target is text-first. M3 is supported for the following consumers:

| Consumer | Contract | Scope |
| --- | --- | --- |
| Consolidated metadata and enabled legacy text stages | Chat completion, optionally strict structured JSON | Included |
| Document-type preclassification | Text chat completion | Included |
| Consensus/review checks | Text chat completion | Included |
| Current OCR prompt tester | Text-only chat wrapper over sample text; it does not invoke OCR or send an image | Included |
| Consolidated metadata prompt tester | Structured text chat planned by #369; no such consumer exists yet | Included when implemented |
| Document Chat | Text chat completion with the default text-provider tuning | Included |
| Provider connection test | Small text chat completion using the draft/saved provider tuning | Included |
| Model discovery | `GET /v1/models` must expose the exact served ID | Included control plane |
| OCR and other page-image stages | MinerU or Ollama vision contract | Excluded |
| Generic OpenAI-compatible vision request to M3 | Informational live probe only | Not a release gate |

Provider tuning must resolve identically for worker and API consumers. #368
owns that shared wiring; this ADR defines the behavior it must preserve.

#### Thinking semantics

The official template accepts `thinking_mode` with exactly `disabled`,
`adaptive`, or `enabled`. Archivist resolves provider tuning first and sends
the resulting mode on every M3 request inside
`chat_template_kwargs`; it does not send the model-card shorthand `thinking`
and does not repurpose OpenAI's top-level `reasoning_effort` field.

| Provider/effective Archivist value | M3 request behavior |
| --- | --- |
| Provider tuning absent (`None`, inherits effective `off`) | `chat_template_kwargs.thinking_mode = "disabled"` |
| Explicit `off` | `chat_template_kwargs.thinking_mode = "disabled"` |
| `low` | `chat_template_kwargs.thinking_mode = "adaptive"` |
| `medium` | `chat_template_kwargs.thinking_mode = "enabled"` |
| `high` | `chat_template_kwargs.thinking_mode = "enabled"` |

The existing `ProviderTuning` contract defines `None` as inherit-Off and the
effective-tuning resolver collapses it to concrete `off`. The M3 production
path must preserve that resolution through worker and API consumers; it must
not reinterpret a missing provider value as the template's adaptive default.
An unresolved library-level `ChatRequest` with no reasoning value is not a
valid fully configured M3 application request.

M3 has no separate medium/high reasoning budget in this contract. Those two
Archivist values are intentional aliases. `max_output_tokens` remains an
independent total output cap and must leave enough room for reasoning plus the
final answer.

Rejecting `chat_template_kwargs` is a runtime-contract failure. Archivist must
return an actionable provider error and must not silently retry without the
field, because that would turn an explicit `off` into the template's adaptive
default.

With the pinned `minimax-m3` reasoning parser, the canonical response contains
the final answer in `message.content` and the trace in
`message.reasoning_content`. The trace may remain in the existing redacted raw
artifact but never becomes the application result. As defense in depth for a
missing or regressed server parser, response extraction removes both
`<think>...</think>` and `<mm:think>...</mm:think>` blocks from content,
including multiple blocks. An opening tag without a close discards the tail.
A response with reasoning but no final answer is a typed contract error with a
reasoning-parser hint, not a successful empty result.

#### Vision and OCR gate

The target checkpoint contains a vision tower, but the M3 preset must not set a
default vision model and must not route the OCR stage to M3. Under this
text-first decision, #371 records the synthetic image contract as
informational only.

Promoting M3 vision/OCR to release scope requires all of the following:

1. a separate decision updating this ADR and the consumer matrix;
2. a pinned SGLang image/model live contract for the exact image payload used
   by Archivist;
3. successful representative OCR/vision quality and capacity evaluation;
4. completion of OCR epic #322 through #338, with at least the data-loss and
   table-integrity blockers #323, #324, #326, and #327 closed.

This links the OCR work rather than duplicating it. MinerU/Ollama remains the
OCR architecture until the gate is explicitly passed.

### Migration and compatibility

- Existing SGLang/MiniMax M2.7 providers remain valid generic
  `openai_compatible` configurations. They are not renamed or migrated
  automatically.
- #370 adds M3 as a disabled preset/catalog choice. Operators opt in and keep
  control of provider URL and secret references; no private endpoint belongs
  in source defaults.
- No database migration or new provider kind is needed.
- The 2026-07-07 M2.7/MinerU design remains authoritative for the
  one-kind-per-protocol rule, MinerU OCR, structured-output fallback, and
  output limits. This ADR replaces its target-model identity, M2-only thinking
  syntax, and the statement that no SGLang chat-template kwargs are needed.

### Rejected alternatives

- **A new `sglang` provider kind:** rejected because it duplicates the existing
  OpenAI-compatible client and confuses protocol with product.
- **Enable M3 vision/OCR immediately:** rejected because model capability is
  not an OCR quality/reliability proof and the linked OCR data-loss issues are
  still open.
- **Send OpenAI `reasoning_effort`:** rejected because the reviewed M3 template
  consumes `thinking_mode`, not OpenAI's field.
- **Use the template's adaptive default for absent tuning:** rejected because
  Archivist already defines absent provider tuning as inherit-Off; changing it
  only for M3 would make provider behavior depend on the call path.
- **Trust the server parser only:** rejected because parser misconfiguration
  would otherwise leak `<mm:think>` content into metadata or Document Chat.

### Risks and release gates

- Engine support is preview/image-specific. Digest pinning and the six-part
  live contract in #371 are mandatory until an equivalent tagged SGLang
  release is adopted deliberately.
- Thinking plus strict JSON grammar and tool parsing can regress independently;
  #371 tests each contract separately and #367 supplies offline wire/parser
  tests.
- Context and concurrency can exhaust KV/activation memory long before the
  architecture maximum; #373 owns measured limits.
- The model is uncensored. Existing authorization, audit, prompt, and document
  access controls remain mandatory; no separate bypass is introduced.

Authoritative public references:

- [MiniMax M3 model card](https://huggingface.co/MiniMaxAI/MiniMax-M3)
- [MiniMax M3 official chat template](https://huggingface.co/MiniMaxAI/MiniMax-M3/blob/main/chat_template.jinja)
- [SGLang MiniMax M3 cookbook](https://docs.sglang.io/cookbook/autoregressive/MiniMax/MiniMax-M3)
- [Target NVFP4 model card](https://huggingface.co/ressl/MiniMax-M3-uncensored-NVFP4)
