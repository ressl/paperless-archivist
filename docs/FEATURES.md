# Feature List

Status: current product feature map
Goal: combine the useful ideas from Paperless-GPT, Paperless-AIssist, and
Paperless-AI into one reliable Rust/PostgreSQL 18 companion for Paperless-ngx.

Hard requirements:

- implementation language: Rust
- database: PostgreSQL 18
- local LLM support: Ollama
- API LLM support: OpenAI, Anthropic Claude, and OpenAI-compatible endpoints
- frontend: React + TypeScript
- frontend/backend boundary: clean OpenAPI boundary
- deployment shape: API/UI service plus worker service
- configuration: all normal runtime settings editable from the UI
- deployment: Kubernetes-friendly and Docker Compose-friendly
- security: user login, RBAC, audit logging, and secure defaults
- optional Paperless-ngx login bridge without storing Paperless passwords

## 1. Core Promise

Paperless Archivist should answer three questions clearly:

1. What can AI improve for this document?
2. What has already been processed?
3. What is still open across the whole archive?

It must support both manual review and full automation.

## 2. Feature Sources

### 2.1 Inspired by Paperless-GPT

Useful capabilities to keep:

- LLM/Vision OCR for bad scans.
- Separate OCR trigger tag.
- Automatic title and tag suggestions.
- Custom prompt templates.
- Local Ollama support.
- Optional external OCR providers later.
- OCR completion tag.
- Manual and automatic processing tags.

### 2.2 Inspired by Paperless-AIssist

Useful capabilities to keep:

- modular trigger tags per stage
- separate text and vision models
- OCR error correction stage
- title generation
- correspondent classification
- document type classification
- document date extraction
- tag classification
- custom field extraction
- prompt management UI
- process queue UI
- preview before applying
- German UI support
- optional Paperless login integration
- one full-pipeline trigger tag
- completion/status tag swap after processing

### 2.3 Inspired by Paperless-AI

Useful capabilities to keep:

- automated processing of new documents
- rules to limit which documents are processed
- broad provider support through OpenAI-compatible APIs
- manual processing screen
- document chat/RAG with source citations
- dashboard for processing state
- provider usage, token, cost, and latency reporting
- configurable prompts and model behavior
- document language detection and configurable tag output language

## 3. Implemented Core Features

### 3.1 Paperless Connection

- Connect to Paperless-ngx via REST API.
- Test connection from UI/API.
- Read documents, tags, correspondents, document types, and custom fields.
- Download original document files for OCR.
- Patch document content and metadata through Paperless API.
- Never write directly to the Paperless database.

### 3.2 PostgreSQL 18 State Engine

- Use PostgreSQL 18 as mandatory database.
- Store all job state in PostgreSQL.
- Store prompt versions.
- Store model/provider configuration.
- Store AI outputs and normalized suggestions.
- Store review decisions.
- Store audit events.
- Store document inventory and backlog state.
- Use `uuidv7()` for Archivist IDs.
- Use PostgreSQL row locking for safe multi-worker processing.

### 3.3 UI Configuration

The UI must allow users to configure all normal runtime behavior.

Configurable in UI:

- Paperless connection settings
- AI providers
- text models
- vision models
- per-stage provider/model selection
- prompts and prompt versions
- workflow trigger tags
- workflow completion tags
- OCR page limits
- review/autopilot mode
- old tag removal/preservation strategy
- new tag creation policy
- retry policy
- batch processing rules
- tag-based include/exclude workflow rules
- dashboard/reporting preferences
- rule definitions

Not configured as normal UI settings:

- database URL
- HTTP bind address
- low-level logging bootstrap
- initial admin/bootstrap token

Secrets:

- UI should accept and test provider credentials.
- Stored secrets must be handled separately from normal settings.
- Kubernetes deployments should prefer Kubernetes secrets.
- Docker Compose deployments should support Docker secrets, mounted files, or
  environment variables.

### 3.4 Deployment Modes

Kubernetes-friendly:

- separate API/UI and worker deployments
- built React assets served by API/UI or optional separate frontend deployment
- stateless application containers
- PostgreSQL 18 external or bundled by platform
- health/readiness endpoints
- metrics endpoint
- horizontal worker scaling
- graceful shutdown
- NetworkPolicy friendly
- rootless container
- read-only root filesystem where practical

Docker Compose-friendly:

- API/UI service
- worker service
- PostgreSQL 18 service
- optional Ollama service
- built React assets included in the API/UI container
- `.env` bootstrap
- named volumes
- simple upgrade path
- same runtime UI settings as Kubernetes

### 3.5 Authentication and Users

Required features:

- local user login
- initial admin bootstrap
- password login with Argon2id hashing
- server-side sessions
- logout
- session revocation
- password change
- failed login tracking
- disabled users
- optional Paperless-ngx login bridge later
- OIDC SSO with ZITADEL using Authorization Code + PKCE

User management:

- create user
- disable user
- assign roles
- reset password
- revoke sessions
- view last login

### 3.6 RBAC

Required roles:

```text
viewer
reviewer
operator
admin
auditor
```

Permissions:

- viewer: read dashboards and runs
- reviewer: approve/reject/edit review items
- operator: start/pause/retry jobs and batches
- admin: manage settings, prompts, providers, users, secrets
- auditor: read audit/security logs

### 3.7 Security Audit

Audit these events:

- login success/failure
- logout
- session revoked
- user created/disabled
- role changed
- API token created/revoked
- settings changed
- provider secret changed
- prompt changed/activated
- batch started/paused/cancelled
- job retried/cancelled
- review approved/rejected/edited
- document patch applied

### 3.8 API Tokens

Required features:

- create scoped token
- show token only once
- store token hashed
- revoke token
- optional expiry
- last-used timestamp
- audit use and revocation

Example scopes:

```text
runs:read
runs:write
reviews:read
reviews:write
settings:read
settings:write
users:manage
inventory:read
batches:write
```

### 3.9 Document Inventory

Archivist must scan all Paperless documents and maintain an inventory.

For every document show:

- Paperless document ID
- title
- current tags
- OCR status
- tagging status
- title status
- correspondent status
- document type status
- document date status
- custom fields status
- current run status
- last successful run
- last error
- next required stage
- whether it needs review
- whether it is complete

Inventory statuses:

```text
unknown
not_needed
queued
running
waiting_review
succeeded
failed
skipped
stale
```

### 3.10 Backlog Dashboard

Must show:

- total documents
- documents fully complete
- documents missing OCR
- documents missing tagging
- documents missing title generation
- documents missing correspondent classification
- documents missing document type classification
- documents missing document date extraction
- documents missing custom field extraction
- documents waiting for review
- documents failed
- documents currently running
- documents never seen by Archivist

Required actions:

- queue all missing OCR jobs
- queue all missing tagging jobs
- queue full pipeline for all incomplete documents
- retry failed jobs
- pause processing
- resume processing
- export backlog as CSV/JSON

### 3.11 Trigger Tags

Default trigger tags:

```text
ai-process
ai-ocr
ai-tags
ai-title
ai-correspondent
ai-document-type
ai-document-date
ai-fields
```

Behavior:

- Archivist creates missing trigger tags automatically.
- Adding a trigger tag in Paperless queues the matching job.
- `ai-process` queues the full configured pipeline.
- Multiple trigger tags can be combined.
- Trigger tags are removed after successful processing.

### 3.12 Completion and Status Tags

Default completion tags:

```text
ai-processed
archivist-ocr
archivist-tags
ai-processed-title
ai-processed-correspondent
ai-processed-document-type
ai-processed-document-date
ai-processed-fields
```

Default attention tags:

```text
ai-review-needed
ai-failed
ai-failed-ocr
ai-failed-tagging
```

Behavior:

- Archivist creates missing workflow tags automatically.
- Completion tags are added by code after success.
- AI prompts must never create completion tags.
- Failure tags are configurable.
- Completion tags are used by the inventory to know what is still open.

### 3.13 OCR

OCR features:

- run vision OCR using Ollama first
- use a separate vision model from the text model
- support page limits
- support force re-OCR
- support skip-if-already-complete
- store OCR text artifact
- update Paperless document content
- mark `archivist-ocr` when done
- show OCR status in backlog

Recommended default model setup:

```text
Vision OCR: qwen2.5vl:7b
Text/tagging: qwen3:8b or a larger tuned Qwen model
Commercial Ollama API: qwen3-vl:235b-instruct for OCR/vision and glm-5.1 for text
```

Later:

- generate searchable replacement PDF with text layer
- support Docling server
- support Azure Document Intelligence
- support Google Document AI

### 3.14 OCR Fix

Optional stage after OCR:

- fix obvious OCR errors
- normalize broken line breaks
- preserve numbers, dates, invoice IDs, policy IDs
- never summarize instead of transcribing
- keep original OCR artifact and fixed OCR artifact separately

### 3.15 Tagging

Tagging features:

- select from existing Paperless tags
- max tags configurable, default 5
- exclude workflow tags from candidates
- optionally allow new tags
- new tags require review by default
- support old-tag strategies:
  - keep existing tags
  - replace AI-managed tags only
  - remove all old business tags
  - preserve configured tags such as `Inbox`
- mark `archivist-tags` after success

Tag validation:

- no unknown tag in autopilot unless `allow_new_tags=true`
- no workflow tag as business tag
- no more than configured max tags
- no empty/noisy OCR terms
- confidence threshold configurable

### 3.16 Title Generation

Features:

- generate concise human-readable document title
- configurable language
- configurable max length
- preserve original filename in Paperless
- review mode by default for title changes
- mark `ai-processed-title` after success

### 3.17 Correspondent Classification

Features:

- classify against existing Paperless correspondents
- optionally suggest new correspondent
- new correspondent requires review by default
- mark `ai-processed-correspondent` after success

### 3.18 Document Type Classification

Features:

- classify against existing Paperless document types
- optionally suggest new document type
- new document type requires review by default
- mark `ai-processed-document-type` after success

### 3.19 Document Date Extraction

Features:

- extract the Paperless document date / issue date as ISO `YYYY-MM-DD`
- prefer issue, invoice, letter, contract, statement, and certificate dates
- avoid scan, upload, processing, delivery, payment due, and reminder due dates
- use detected document language and multilingual date formats
- route ambiguous or low-confidence dates through review
- protect existing Paperless document dates unless overwrite is explicitly enabled
- mark `ai-processed-document-date` after success

### 3.20 Custom Field Extraction

Features:

- extract structured fields from document content
- support global prompts
- support document-type-specific prompts
- merge global and type-specific extraction results
- validate field types before applying
- mark `ai-processed-fields` after success

Useful fields:

- invoice date
- due date
- invoice number
- total amount
- currency
- tax amount
- customer number
- insurance policy number
- IBAN
- reference number

### 3.20 Review Mode

Review mode features:

- show pending AI suggestions
- show current Paperless metadata
- show suggested patch
- approve
- reject
- edit then approve
- retry with different prompt/model
- apply selected stages only

Required review metadata:

- document ID
- stage
- model
- prompt version
- confidence
- generated at
- diff before/after
- validation warnings

### 3.21 Workflow Modes

Archivist supports three workflow modes: manual trigger with manual review,
automatic document selection with manual review, and full autopilot. Full
autopilot applies changes automatically only if validation passes.

Must support:

- per-stage enable/disable
- confidence threshold
- allow/disallow new tags
- allow/disallow metadata overwrite
- fallback to review on validation failure
- dry-run mode

### 3.22 Batch Processing All Documents

Required because the tool must process an existing archive.

Features:

- scan all Paperless documents
- calculate missing stages per document
- create jobs for missing OCR
- create jobs for missing tagging
- create jobs for full pipeline
- respect completion tags
- respect existing run history
- support filters:
  - document ID range
  - created date range
  - tag
  - correspondent
  - document type
  - missing stage
  - failed stage
- pause/resume batch
- show progress
- continue after errors
- retry failed subset

### 3.23 Rules

Rules decide when and how to process.

Examples:

- only OCR documents without `archivist-ocr`
- do not process documents with tag `Private`
- use a different prompt for `Rechnung`
- always review documents with low confidence
- never auto-create tags in autopilot
- preserve `Inbox` tag until human review
- skip OCR for digital PDFs if content is already good enough

### 3.24 Prompt Management

Features:

- versioned prompts
- active prompt per stage
- prompt test runner
- sample document preview
- rollback prompt version
- compare output between prompt versions
- show all runs using a prompt version

Prompt types:

```text
vision_ocr
ocr_fix
title
tag
correspondent
document_type
fields
classify_combined
```

### 3.25 Model Management

Features:

- configure providers
- configure text model
- configure vision model
- per-stage model override
- provider health check
- provider cost-per-token configuration for reporting
- installed local Ollama model discovery through the backend
- manual refresh and inline error state for Ollama model lists
- missing installed-model warning while preserving the stored setting
- hardware recommendation tooltip sourced from a data file
- model test prompt
- latency and error metrics
- usage, token, and estimated-cost dashboard
- local Ollama first
- OpenAI API
- Anthropic Claude API
- OpenAI-compatible API
- separate OCR/tagging/title/field models

Required provider capabilities:

```text
ollama              local text and vision models
openai              text and vision-capable API models
anthropic           Claude text and vision-capable API models
openai_compatible   vLLM, LiteLLM, LM Studio, OpenRouter, local gateways
```

Provider rules:

- each stage can select its own provider/model
- local Ollama is the default
- external API providers are opt-in
- secrets are never stored in plain DB settings
- model responses are normalized into Rust domain types before validation

### 3.26 Audit Log

Every applied change must be auditable.

Record:

- actor (`worker`, user ID, API token name)
- run ID
- job ID
- document ID
- stage
- model
- prompt version
- before value
- after value
- validation result
- timestamp

Audit logs must be visible per document and globally.

### 3.27 Reporting

Reports:

- backlog by stage
- failures by stage
- failures by model
- documents processed per day
- average OCR duration
- average tagging duration
- review approval rate
- autopilot success rate
- prompt version quality comparison
- tag distribution before/after AI

Exports:

- CSV
- JSON

### 3.28 Document Chat and RAG

Features:

- stored chat sessions
- stored user and assistant messages
- optional Paperless document ID filter
- retrieval from Paperless document content via REST API
- bounded source snippets
- archive-wide chat over inventory candidates
- source document citations
- audit events for chat sessions and messages

Embedding-backed semantic search and page-level citations can be added later
without changing the frontend/backend boundary.

## 4. User Workflows

### 4.1 Process One Document

1. User adds `ai-process` tag in Paperless or clicks trigger in Archivist.
2. Archivist creates a pipeline run.
3. Worker runs configured stages.
4. Suggestions are either reviewed or auto-applied.
5. Trigger tag is removed.
6. Completion tags are added.
7. Audit log records changes.

### 4.2 Process All Old Documents

1. User opens backlog dashboard.
2. User clicks "scan archive".
3. Archivist syncs all Paperless documents into inventory.
4. Dashboard shows missing OCR/tagging counts.
5. User starts batch for missing OCR.
6. User starts batch for missing tagging.
7. Failed jobs remain visible and retryable.
8. Completed jobs get completion tags.

### 4.3 Review Suggestions

1. User opens review queue.
2. User selects a document.
3. UI shows current metadata and AI suggestion.
4. User edits tags/title/type if needed.
5. User approves.
6. Archivist patches Paperless and writes audit log.

## 5. Current Product Cut And Next Phase

Implemented product path:

- PostgreSQL 18 schema
- UI-managed settings
- login and RBAC
- audit logging
- scoped API tokens
- Docker Compose deployment
- containerized production deployment support
- React application shell
- generated OpenAPI client
- Paperless client
- Ollama text provider
- Ollama vision provider
- document chat/RAG
- document inventory
- backlog dashboard API and UI
- provider usage/cost/latency dashboard
- prompt test runner
- batch review actions
- audit CSV export
- optional Paperless-ngx login bridge
- tag-based workflow rules
- worker queue
- OCR stage
- tagging stage
- title/correspondent/document type stages
- custom fields
- completion tags
- review API
- review queue UI
- autopilot mode
- audit log

Next product phase:

- advanced prompt editor
- advanced reporting views
- keyboard-driven batch review ergonomics
- provider quality scoring dashboards

Future extensions:

- PDF text layer replacement
- embedding-backed semantic retrieval
- advanced providers
- mobile-friendly review UI
- full Paperless auth integration
