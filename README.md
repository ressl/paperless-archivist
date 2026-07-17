# Paperless Archivist - AI OCR, Tagging, RAG, and Secure Automation for Paperless-ngx

![Paperless Archivist logo](assets/brand/paperless-archivist-logo.png)

[![CI](https://github.com/ressl/paperless-archivist/actions/workflows/ci.yml/badge.svg)](https://github.com/ressl/paperless-archivist/actions/workflows/ci.yml)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/backend-Rust-orange.svg)](Cargo.toml)
[![React](https://img.shields.io/badge/frontend-React%20%2B%20TypeScript-61dafb.svg)](frontend/package.json)
[![Paperless-ngx](https://img.shields.io/badge/Paperless--ngx-companion-green.svg)](https://github.com/paperless-ngx/paperless-ngx)

Paperless Archivist is a security-first AI automation layer for
[Paperless-ngx](https://github.com/paperless-ngx/paperless-ngx). It adds AI OCR,
AI tagging, metadata extraction, document chat/RAG, review queues, autopilot,
dashboards, login, RBAC, audit events, and UI-managed runtime settings without
writing directly to the Paperless database.

It is built for Paperless-ngx users who want local Ollama, Ollama Cloud, OpenAI,
Anthropic, or OpenAI-compatible document AI without giving the browser direct
access to Paperless or model providers. Every write goes through the Paperless
REST API, every important action is auditable, and model output can be reviewed
before it touches your archive.

Paperless Archivist is implemented as a Rust backend and Rust worker, a
React + TypeScript frontend, and PostgreSQL 18 storage. That stack is chosen for
reliable long-running jobs, strong type boundaries, predictable deployment, and
safe operation around private documents.

Common search terms this project is designed to solve naturally:
Paperless-ngx AI, Paperless AI OCR, Paperless-ngx tagging automation, Paperless
Ollama integration, Paperless-ngx RAG, Paperless document chat, AI metadata
extraction for Paperless, and secure AI automation for Paperless-ngx.

## Why Archivist

Most Paperless AI helpers focus on one strong feature: better OCR, metadata
suggestions, or chat. Paperless Archivist is built around the operational
workflow:

- know what is open, running, failed, waiting for review, and complete
- run OCR and tagging jobs for one document or a full backlog
- keep jobs resumable and idempotent
- validate model output before applying it
- route risky changes through review
- switch to autopilot only after the pipeline is configured and trusted
- add completion tags and remove trigger tags after successful applies
- manage Paperless, model providers, prompts, users, sessions, and API tokens in
  the UI
- keep a full audit trail for settings, security, review, job, and apply actions

## Why Paperless-ngx Users Want It

Paperless-ngx already gives you a strong document management system. Archivist
adds the missing operational AI layer around it:

- **Less manual filing:** AI can suggest tags, titles, correspondents, document
  types, custom fields, and improved OCR text.
- **Local-first privacy:** run local Ollama models for private archives, or opt
  into commercial providers only when your policy allows it.
- **Human control:** use review mode until the pipeline is trusted, then enable
  validation-gated autopilot.
- **Backlog visibility:** see what is open, running, failed, waiting for review,
  and complete before the archive becomes messy.
- **Document chat:** ask questions across retrieved Paperless documents and keep
  citations back to source documents.
- **Operational safety:** jobs are resumable, applies are idempotent where
  practical, and completion/trigger tags make the workflow visible in Paperless.
- **Runtime configuration:** configure Paperless, model providers, prompts,
  workflow mode, users, sessions, and API tokens from the UI.

## Security Advantages

Archivist is designed for private archives, invoices, contracts, insurance
documents, tax records, and other sensitive paperwork.

| Security boundary | What Archivist does |
| --- | --- |
| Paperless database safety | Never writes directly to the Paperless database; all changes use the Paperless REST API |
| Browser isolation | The frontend only talks to Archivist, never directly to Paperless or AI providers |
| Secret handling | Paperless and provider credentials are stored as encrypted secret references and redacted from responses/audit metadata |
| Login and access control | Local login, Argon2id password hashing, server-side sessions, CSRF, OIDC SSO, RBAC roles, and scoped API tokens |
| Review before apply | Model output can be approved, rejected, or edited before it changes Paperless metadata |
| Validation before autopilot | Full autopilot applies only validated Rust domain patches; invalid output can fall back to review |
| Auditability | Settings, security, prompt, review, job, chat, and apply actions write audit events |
| Local AI option | Ollama support lets you keep document content on your own infrastructure |

The goal is not just to "call an LLM"; it is to make Paperless-ngx AI automation
safe enough to run repeatedly on a real archive.

## Core Features

| Area | What you get |
| --- | --- |
| Paperless integration | REST API only, no direct database writes, document inventory sync, tag and metadata patching |
| Deep Paperless sync | Full sync plus optional modified-since delta sync with overlap, active archive profile preparation, and metadata consistency checks |
| OCR pipeline | Vision/OCR stage with configurable models and resumable worker jobs |
| Tagging pipeline | Title, correspondent, document type, document date, tag, and field suggestions with Rust-side validation |
| Language intelligence | Local language detection stores BCP-47 tags, feeds prompt context, and lets operators choose the language for newly generated tags |
| Internationalized UI | Browser-language startup, persisted UI language selector, complete English/German catalogs, and fallback entries for ISO 639-1 world languages |
| Review flow | Approve, reject, or edit AI suggestions before they are applied |
| Workflow modes | Manual trigger + review, autopilot selector + review, or full autopilot |
| Autopilot | Automatic document selection and optional automatic apply after validation; failures can fall back to review |
| Completion tags | Add `archivist-ocr`, `archivist-tags`, and `ai-processed` after successful stages |
| Trigger cleanup | Remove trigger tags after the corresponding stage succeeds |
| Dashboard | Backlog status, completion rate, failure rate, throughput, review load, timelines, and status charts |
| Document chat/RAG | Ask questions against retrieved Paperless document content with stored chat sessions and cited sources |
| Runtime settings | Configure Paperless, providers, prompts, workflow mode, users, sessions, and API tokens from the UI |
| Maintenance tools | Dry-run completion tag reconciliation, safe bulk apply, recovery tools, and Paperless inventory consistency checks |
| Setup and notifications | First-run setup wizard plus webhook notifications for review backlog, repeated failures, and paused full autopilot |
| Workflow rules | Include/exclude batch-processing rules by Paperless tags |
| Security | Argon2id, sessions, CSRF, RBAC, scoped API tokens, OIDC SSO, optional Paperless login bridge, secret redaction, audit logging |
| Deployment | Hardened Docker Compose profiles and a public-safe generic Kubernetes package |

## Screens and Workflow

```text
Paperless-ngx  ->  Archivist inventory  ->  Job queue  ->  OCR/tagging
                                                        ->  Review queue
                                                        ->  Validated apply
                                                        ->  Completion tags
```

1. Connect Archivist to Paperless-ngx with a Paperless API token.
2. Sync the document inventory.
3. Configure local Ollama or a commercial model provider.
4. Queue OCR, tagging, or the full pipeline.
5. Review suggestions or enable autopilot for validated changes.
6. Track the backlog and job health on the dashboard.

The frontend only talks to Archivist. Archivist is the policy and audit
boundary between your browser, Paperless-ngx, and model providers.

## Interface Languages

The frontend starts with the browser language, stores the selected UI locale in
the browser, and exposes a language selector on the login screen, sidebar, and
Settings page. English and German are maintained as complete catalogs. Other
ISO 639-1 world languages appear in the selector with their native names and
use the English fallback until a translation catalog is added.

Numbers, percentages, relative times, and operational dates are formatted with
the selected locale. Document processing language is separate from UI language:
document language is detected from OCR/content, while `tag_output_language`
controls the language for newly generated business tags.

## Quick Start

Requirements:

- Docker with Compose support
- a running Paperless-ngx instance
- a Paperless API token
- optional Ollama if you want local inference

```bash
cp deploy/compose/.env.example deploy/compose/.env
$EDITOR deploy/compose/.env
docker compose --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

Open:

```text
http://127.0.0.1:8080
```

Log in with `ARCHIVIST_ADMIN_USERNAME` and
`ARCHIVIST_ADMIN_PASSWORD` from your environment file.

To include Ollama in the same Compose stack:

```bash
docker compose --profile ollama --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

More deployment options:

- [Docker Compose profiles](deploy/compose/README.md)
- [Generic Kubernetes package](deploy/kubernetes/README.md)
- [Migration and rollback guide](docs/MIGRATIONS.md)
- [Performance and sizing](docs/PERFORMANCE.md)
- [Public demo plan](docs/DEMO.md)
- [Screenshot workflow](docs/SCREENSHOTS.md)

## Documentation

| Guide | Audience |
| --- | --- |
| [Installation Guide](docs/INSTALLATION.md) | First install, Compose, Kubernetes, local development |
| [User Guide](docs/USER_GUIDE.md) | Operators, reviewers, and admins using the UI |
| [Feature Reference](docs/FEATURE_REFERENCE.md) | Complete product capability map |
| [Operations Guide](docs/OPERATIONS.md) | Deploying, backing up, monitoring, and troubleshooting |
| [API Reference](docs/API_REFERENCE.md) | API clients and integration authors |
| [Security Guide](docs/SECURITY.md), [Security Design](docs/SECURITY_DESIGN.md), and [Security Policy](SECURITY.md) | Secure operation, review, and vulnerability reporting |
| [AI Agent Guide](docs/AI_AGENT_GUIDE.md) | AI coding agents and contributors |
| [FAQ](docs/FAQ.md) and [Troubleshooting](docs/TROUBLESHOOTING.md) | Common questions and failure modes |
| [Stability Policy](docs/STABILITY.md) | v1.0 compatibility and support expectations |
| [Accessibility Audit](docs/ACCESSIBILITY.md) | Keyboard, focus, tooltip, and screen-reader checks |
| [Performance Guide](docs/PERFORMANCE.md) | Large archive sizing and benchmark workflow |
| [Migration Guide](docs/MIGRATIONS.md) | Fresh install, upgrade, backup, and rollback |
| [Release Checklist](docs/RELEASE_CHECKLIST.md) | GA release verification |
| [Roadmap](docs/ROADMAP.md) | Known limitations and upcoming work |

## First Run Checklist

1. Open `Settings`.
2. Set the Paperless base URL and Paperless API token.
3. Choose an AI provider.
4. For OpenAI, Anthropic, or Ollama Cloud, enter the API key.
5. Test the Paperless and provider connections.
6. Run `Sync` on the dashboard. Enable delta sync later if full sync is stable.
7. Check Paperless consistency and run completion-tag reconcile as a dry run.
8. Queue one OCR job and one tagging job.
9. Review the output.
10. Enable autopilot only after the validation results match your archive rules.

## Paperless Maintenance

The dashboard includes admin maintenance tools for real archives:

- `Sync` reads documents, tags, correspondents, document types, document dates,
  modified timestamps, and custom fields through the Paperless REST API.
- Optional delta sync uses Paperless modified timestamps with a configurable
  overlap window, so routine refreshes can avoid scanning the full archive.
- The consistency checker compares Paperless documents with Archivist inventory
  and reports missing local documents, stale local documents, and metadata
  mismatches.
- Completion-tag reconcile is dry-run first. It plans documents that already
  have all enabled stage completion tags but still miss the full completion tag,
  then lets an admin apply the planned Paperless tag update deliberately.
- Custom-field mappings let operators disable fields, add aliases, and give the
  extraction prompt field-specific instructions without changing the database
  schema.

## Model Providers

Archivist ships with sensible provider defaults. Commercial providers only need
an API key unless you want to override model choices.

| Provider | Default text model | Default vision/OCR model | Best fit |
| --- | --- | --- | --- |
| Ollama | `qwen3:8b` | `qwen2.5vl:7b` | Local-first private inference |
| Ollama Cloud | `glm-5.1` | `qwen3-vl:235b-instruct` | Hosted Ollama API with strong default models |
| OpenAI | `gpt-5.5` | `gpt-5.5` | Highest-quality hosted general reasoning and vision |
| Anthropic | `claude-sonnet-4-6` | `claude-sonnet-4-6` | Hosted reasoning, extraction, and long-context review |
| OpenAI-compatible | `qwen3:8b` | `qwen2.5vl:7b` | Local gateways, vLLM, LiteLLM, LM Studio, or compatible APIs |

Text and vision models are separate dropdowns in the UI. For local Ollama,
Archivist loads the installed models through the backend from Ollama `/api/tags`,
sorts them alphabetically, and shows model name, parameter size, quantization,
and size in GB. If Ollama is unavailable or returns an empty list, Settings shows
a clear inline status and keeps the saved model value selected. If the saved
model is not installed, it remains selectable with a warning.

OpenAI, Anthropic, Ollama Cloud, and OpenAI-compatible providers use the
app-compatible model catalog shipped with the frontend. The local Ollama
dropdown includes a hardware recommendation tooltip; the NVIDIA GeForce RTX
4060 Ti profile currently recommends `qwen3:4b-instruct` for text/LLM and
`glm-ocr` for vision/OCR.

## Document Chat And RAG

Archivist includes a document chat built around the same safety boundary as the
OCR and tagging pipeline.

How it works:

1. Open `Chat` and create a chat session.
2. Ask a question about your archive.
3. Optionally restrict retrieval to known Paperless document IDs.
4. Archivist searches the local inventory cache, fetches candidate document
   content through the Paperless REST API, and builds bounded source snippets.
5. The configured default text provider answers from those snippets.
6. The answer includes document citations such as `[doc:123]`.

Stored in PostgreSQL:

- chat sessions
- user and assistant messages
- retrieved source snippets
- source document IDs and scores
- provider/model metadata
- audit events for session creation and message creation

Security properties:

- the frontend never talks directly to Paperless or model providers
- chat requires an authenticated browser session with reviewer, operator, or
  admin permissions
- document content reaches model providers only through Archivist backend code
- chat actions are auditable
- provider secrets are still stored as encrypted secret references

Current retrieval uses Paperless document content plus inventory metadata.
Embedding-backed semantic search and page-level citations are planned as a
future retrieval upgrade.

## Product Status And Next Phase

Paperless Archivist is past scaffolding: the current product path includes the
Rust API, Rust worker, React + TypeScript UI, PostgreSQL 18 schema, Paperless
REST integration, provider configuration, dashboard, OCR/tagging jobs, review
flow, validation-gated workflow modes, completion tags, audit events, and document
chat. Product-phase additions now include an advanced Prompt Workbench, batch
review actions, provider usage/cost/latency reporting, audit CSV export,
tag-based workflow rules, and an optional Paperless-ngx login bridge.
Full-auto operations include pause/resume, dry-run, hourly/daily document
limits, and dashboard debug context for the selector, Paperless, and model
pipeline. Reliability features include trace IDs across worker logs and live UI,
Prometheus metrics for selector/retry/model/apply health, and operator recovery
tools for stale leases or stuck runs.

## Workflow Modes

| Mode | API value | Document selection | Apply behavior |
| --- | --- | --- | --- |
| Manual trigger + review | `manual_review` | Only explicitly queued documents or Paperless trigger tags | Suggestions wait in the review queue |
| Autopilot selector + review | `auto_select_review` | Archivist automatically queues documents with missing enabled stages | Suggestions wait in the review queue |
| Full autopilot | `full_auto` | Archivist automatically queues documents with missing enabled stages | Valid suggestions are applied to Paperless automatically |

Safety controls are available from Dashboard and Settings:

- **Pause/resume:** stops automatic selector and trigger polling while manual UI
  queue actions remain available.
- **Dry-run:** lets full-auto select and evaluate documents but routes validated
  patches into Review instead of applying them.
- **Hourly/daily limits:** cap how many auto-selected documents can be queued in
  each rolling window.
- **Debugging light:** Dashboard, Inventory, and Review expose selector state,
  next selector scan, detected prompt language, tag output language, run status,
  and safe failure reasons without exposing document text or secrets.

Successful OCR and tagging stages add `archivist-ocr` and `archivist-tags`.
Standard metadata stages can add `ai-processed-correspondent`,
`ai-processed-document-type`, and `ai-processed-document-date`. The final stage
in a run also adds `ai-processed`. Trigger tags are removed after the
corresponding stage succeeds so the worker can resume safely without looping
over already-completed work.

## Language Intelligence

Archivist detects document language locally from OCR/document text and stores a
BCP-47 language tag with confidence in the inventory. Prompt requests include
the detected document language, confidence, and the configured tag output
language so models preserve source-language content while writing newly
generated business tags in the operator-selected language. Existing Paperless
tags, correspondents, document types, dates, names, and identifiers are kept
exact unless a reviewer explicitly changes them.

## Paperless Standard Metadata

Archivist can fill the standard Paperless fields that users normally maintain by
hand:

- **Correspondent:** selected from the synced Paperless correspondents by exact
  name, with confidence and evidence shown in review.
- **Document type:** selected from the synced Paperless document types by exact
  name.
- **Document date:** extracted as ISO `YYYY-MM-DD` from issue, invoice, letter,
  contract, statement, or certificate date evidence. Due dates, scan dates,
  upload dates, and processing dates are treated as risky and routed to review
  unless confidence is high.

The worker only writes through the Paperless REST API. Existing non-empty
standard fields are protected by default; admins can explicitly allow overwrite
in Settings. Review items show the current value, suggested value, confidence,
evidence, warnings, and editable field controls before apply.

## Default Prompt Pack

Archivist ships with versioned defaults for OCR, OCR post-processing, tagging,
title generation, correspondent detection, document type detection, document
date extraction, and custom field extraction. The prompts are installed into
PostgreSQL by migrations, visible in the `Prompts` Workbench, and can be tested,
compared, versioned, or replaced without rebuilding.

The defaults are designed for Paperless archives rather than generic chat:
classification uses exact Paperless metadata names, workflow/control tags are
excluded from business tagging, OCR returns raw text only, and structured stages
return strict JSON that Rust validates before review or apply. See
[Prompt Pack](docs/PROMPTS.md) for the stage-by-stage behavior.

The Prompt Workbench shows the active prompt, all prior versions, usage counts,
stage-specific help, safety notes, a test runner, and version comparison. Edits
create new immutable versions, so teams can tune prompts and roll back without
losing auditability.

Some features were intentionally deferred from the initial product cut so the
core workflow could stay reliable and auditable. They are the next
product-quality layer, not hidden requirements for basic use:

| Area | Current state | Next product phase |
| --- | --- | --- |
| Document chat retrieval | Uses Paperless content and inventory metadata with cited source documents | Embedding-backed semantic search, better ranking, and page-level citations |
| Local model operations | Shows installed Ollama models, keeps saved values, and provides hardware recommendations | Optional guided `ollama pull`, more GPU profiles, and model fit guidance by archive size |
| Review ergonomics | Approve, reject, edit, and batch-approve or batch-reject suggestions safely | More keyboard-driven review triage for high-volume inboxes |
| Analytics | Backlog KPIs, status charts, timelines, throughput, failure rate, review load, provider usage, token totals, configurable cost estimates, latency, and Prometheus metrics | Quality scoring and provider comparison trends |
| Provider depth | Ollama, Ollama Cloud, OpenAI, Anthropic, and OpenAI-compatible APIs | More provider-specific structured-output adapters and fallback strategies |
| Workflow portability | UI-managed prompts, prompt test runner, workflow rules, and settings | Import/export for prompt packs and workflow profiles |
| Release hardening | Docker Compose, CI checks, dependency audits, and container build checks | Signed release artifacts, richer upgrade checks, and production deployment packages managed outside the public source tree |

The core safety rules are already product rules: no direct Paperless database
writes, no frontend-to-provider calls, no unreviewed autopilot apply without
validation, and audit events for privileged actions.

## Security Governance

Paperless Archivist is implemented in Rust for the backend, worker, domain
logic, Paperless client, AI provider adapters, and database access, with a
React and TypeScript frontend. The security model is enforced server-side:
Argon2id password hashing, HTTP-only sessions, CSRF protection, RBAC, scoped API
tokens, audit logging, secret redaction, and no direct browser calls to
Paperless or model providers.

Product hardening includes API token expiry and rotation, configurable audit
and AI-artifact retention, AI artifact privacy modes, and audit hash-chain
verification. The default artifact mode is `redacted`, which keeps operational
telemetry while removing document text, prompts, images, and raw model text from
stored AI artifacts.

## AI Quality And Evaluation

Paperless Archivist treats model output as measured workflow evidence, not as a
black box. The dashboard includes quality telemetry for review decisions,
acceptance rate, edits, rejections, uncertainty-routed reviews, provider/model
feedback, token volume, latency, and optional cost estimates. Operators can use
these signals to compare models, spot prompt regressions, and decide when a
workflow is safe enough for more automation.

The repository also contains a public-safe golden document harness and prompt
regression tests. Golden fixtures cover language and issue-date extraction, and
prompt regression tests verify security wording, language context, strict JSON
output, and deterministic temperature settings. Prompt changes should update
the fixture coverage and [Prompt Release Notes](docs/PROMPT_RELEASE_NOTES.md).

## Comparison With Other Paperless AI Projects

This matrix is based on public project documentation checked on 2026-05-14.
Projects move quickly, so treat it as a practical orientation rather than a
permanent scorecard.

| Capability | Paperless Archivist | [paperless-gpt](https://github.com/icereed/paperless-gpt) | [Paperless-AI](https://github.com/clusterzx/paperless-ai) | [Paperless-AIssist](https://github.com/nyxtron/paperless-aissist) | [Taan Mind](https://taan-mind.com/) | Paperless-ngx built-in |
| --- | --- | --- | --- | --- | --- | --- |
| Main focus | Operated AI pipeline with review, audit, dashboard, and safe apply | LLM-enhanced OCR and AI metadata suggestions | Auto-classification, smart tagging, and RAG chat | Web-UI configured AI middleware with vision OCR and document chat | Document AI workspace with chat, OCR, enrichment, and KPI views | Core document management system |
| Paperless DB writes | No, REST API only | Uses Paperless API | Uses Paperless API | Uses Paperless API | Uses app proxy/API layer | Owns the Paperless database |
| OCR enhancement | Yes, OCR stage with vision models | Strong focus: LLM OCR plus Azure, Google Document AI, and Docling options | Mostly depends on Paperless OCR | Vision OCR plus OCR post-processing | Ollama OCR plus MuPDF processing | Classical OCR pipeline |
| Tagging and metadata | Titles, tags, correspondent, document type, document date, fields | Titles, tags, created date, correspondent, custom fields | Titles, tags, document type, correspondent | Title, tags, type, correspondent, custom fields | AI metadata enrichment | Matching rules and classifier |
| Review before apply | First-class review queue with approve, reject, edit | Web UI review and auto processing | Manual processing UI | Preview and process queue | Inspect and patch workflow | Native UI review of normal metadata |
| Autopilot | Yes, validation-gated | Yes, via auto tags/processing | Yes, automatic processing rules | Yes, scheduler and trigger tags | Processing pipeline | Native consume/classification automation |
| Resumable worker jobs | Yes, PostgreSQL-backed leases, retries, idempotent apply | Container workflow | Queue/process workflow | Process queue and scheduler | Processing lifecycle | Paperless task queue |
| Completion and trigger tags | Built in, explicit stage tags | OCR complete tag and trigger tags | Rules/output tags | `ai-process` to `ai-processed`, modular tags | Processing labels/status | Native tags, not AI-stage completion |
| Dashboard and metrics | Backlog KPIs, trends, status charts, Prometheus metrics | Processing status UI | Dashboard and RAG UI | Dashboard, logs, prompts, chat | KPI dashboard | Native document dashboard |
| Document chat/RAG | Built-in archive chat with citations from retrieved Paperless content | Ad-hoc analysis, not primary RAG | Strong RAG chat focus | Document chat | Strong workspace/chat focus | Search, no LLM chat |
| UI-managed settings | Yes: providers, secrets, prompts, workflow, users, sessions | Mixed env vars and web UI prompts/settings | Web UI plus configuration | Strong web UI, SQLite config | Full workspace settings | Native Paperless settings |
| Authentication | Local login, OIDC SSO, RBAC, scoped API tokens | App-level controls vary by deployment | Project README highlights workflow more than app auth | Optional Paperless credential auth, disabled by default | Anonymous sessions in app layer | Full Paperless auth model |
| Audit events | Settings, security, jobs, reviews, and applies | Not the central focus | Not the central focus | Processing logs | Operational views | Native history/logging |
| Deployment style | Rust API + worker, PostgreSQL 18, React, Docker Compose | Go service, Docker | Node/JS app, Docker | Python/FastAPI + React, single container | Nuxt/Nitro workspace, Docker-ready | Django/Celery stack |
| Best choice when | You want controlled production workflows, review, audit, and resumable jobs | OCR quality on difficult scans is the main problem | You want document chat/RAG and broad OpenAI-compatible provider support | You want quick web-UI configuration and modular trigger tags | You want a broader document AI workspace | You need the core DMS |

### Short Take

- Choose Archivist when the hard part is not "call an LLM" but operating a
  trustworthy document pipeline.
- Choose paperless-gpt when your biggest pain is OCR quality on poor scans.
- Choose Paperless-AI when document chat and semantic search are the priority.
- Choose Paperless-AIssist when you want a lightweight UI-driven middleware with
  modular tags and type-specific prompts.
- Choose Taan Mind when you want a broader AI workspace around Paperless-ngx.
- Keep using native Paperless-ngx automation when rule-based matching is enough.

## Architecture

```text
React UI
  |
  | OpenAPI-generated client
  v
Rust Axum API  ------ PostgreSQL 18
  |                     |
  | Paperless REST      | jobs, inventory, settings,
  | model APIs          | reviews, audit events
  v                     |
Rust Tokio worker <----+
```

Key properties:

- Rust domain logic validates and normalizes model output before apply.
- PostgreSQL stores inventory, jobs, settings, prompt versions, review items,
  AI artifacts, dashboard snapshots, sessions, API tokens, and audit events.
- The worker can resume jobs after restarts and avoids duplicate applies.
- OpenAPI is the contract between backend and frontend.
- The frontend never talks directly to Paperless-ngx, Ollama, OpenAI,
  Anthropic, or any OpenAI-compatible provider.

## Technology Stack

Paperless Archivist is intentionally implemented with a small, explicit stack:

| Layer | Technology | Why it matters |
| --- | --- | --- |
| Backend API | Rust with Axum and Tokio | memory-safe, fast, reliable for privileged document automation |
| Worker | Rust async worker | resumable OCR/tagging jobs, retries, leases, and idempotent apply behavior |
| Domain logic | Rust | typed validation of AI output before it can modify Paperless metadata |
| Database | PostgreSQL 18 | durable jobs, audit trail, settings, sessions, inventory, reviews, and chat records |
| Frontend | React + TypeScript | typed UI for settings, dashboard, review, security, and document chat |
| API contract | OpenAPI | generated frontend client and stable backend/frontend boundary |
| Deployment | Docker Compose and container image | local self-hosting and production packaging |

## Security Model

Archivist treats document automation as a privileged workflow.

- Argon2id password hashing
- server-side sessions with HttpOnly cookies
- CSRF protection for browser writes
- OIDC SSO with Authorization Code + PKCE
- RBAC roles: viewer, reviewer, operator, admin, auditor
- scoped API tokens
- secret redaction in API responses and audit metadata
- encrypted secret references for Paperless and provider credentials
- audit events for settings, security, review, job, and apply actions
- no direct Paperless database writes
- no direct browser access to Paperless or model providers

## Deployment

Docker Compose:

```bash
docker compose --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

Production Kubernetes manifests are intentionally not stored in this public
source tree. Keep environment-specific manifests, ingress hosts, runtime
secrets, and deployment promotion state in your private deployment repository or
platform automation.


Health and metrics:

```bash
curl http://127.0.0.1:8080/healthz
curl http://127.0.0.1:8080/readyz
```

`/metrics` is disabled until `ARCHIVIST_METRICS_TOKEN` is configured and then
requires that dedicated bearer token. See the
[authenticated monitoring procedure](docs/OPERATIONS.md#metrics); do not place
the token in shell history or a committed manifest.

## Local Development

Backend:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Frontend:

```bash
cd frontend
pnpm install
pnpm generate:client
pnpm typecheck
pnpm build
```

Run the API:

```bash
export DATABASE_URL=postgres://archivist:archivist@127.0.0.1:5432/archivist
export ARCHIVIST_SECRET_KEY=change-me-32-bytes-minimum-local-secret
export ARCHIVIST_ADMIN_PASSWORD=change-me-admin-password
cargo run -p archivist-api
```

Run the worker:

```bash
export DATABASE_URL=postgres://archivist:archivist@127.0.0.1:5432/archivist
export ARCHIVIST_SECRET_KEY=change-me-32-bytes-minimum-local-secret
cargo run -p archivist-worker
```

The API serves the built frontend from `frontend/dist`. During frontend work,
run `pnpm dev` in `frontend/`; Vite proxies `/api` to `127.0.0.1:8080`.

## Documentation

- [Documentation Index](docs/README.md)
- [User Guide](docs/USER_GUIDE.md)
- [AI Agent Guide](docs/AI_AGENT_GUIDE.md)
- [Project Overview](docs/PROJECT_OVERVIEW.md)
- [Operations Guide](docs/OPERATIONS.md)
- [Development Guide](docs/DEVELOPMENT.md)
- [API Reference](docs/API_REFERENCE.md)
- [Security Design](docs/SECURITY_DESIGN.md)
- [Architecture Decisions](docs/ARCHITECTURE_DECISIONS.md)
- [PostgreSQL 18 Design](docs/POSTGRESQL_18_DESIGN.md)
- [Frontend Design](docs/FRONTEND_DESIGN.md)
- [Feature List](docs/FEATURES.md)

## Roadmap

- semantic embeddings for larger archive-wide retrieval
- more provider-specific structured-output adapters
- keyboard-driven review triage for high-volume inboxes
- provider quality scoring and cross-provider comparison views
- import/export for prompt packs and workflow profiles

## Contributing

Contributions are welcome, especially in these areas:

- provider adapters and model presets
- prompt packs for common document types
- validation rules for invoices, contracts, insurance, and tax documents
- dashboard and review workflow improvements
- deployment examples and hardening guides

Start with [CONTRIBUTING.md](CONTRIBUTING.md) and keep changes focused.

## License

Paperless Archivist is licensed under
[AGPL-3.0-or-later](LICENSE).
