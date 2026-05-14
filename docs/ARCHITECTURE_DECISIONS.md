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
