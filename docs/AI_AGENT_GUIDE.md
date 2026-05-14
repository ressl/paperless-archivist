# Paperless Archivist AI Agent Guide

This guide is for AI coding agents, documentation agents, and reviewers working
on Paperless Archivist. It explains the product boundaries, repository layout,
safe implementation patterns, and verification commands.

If instructions in this file conflict with code, tests, or design documents,
prefer the stricter security boundary and update documentation after changing
behavior.

## Product In One Paragraph

Paperless Archivist is a Rust and React companion for Paperless-ngx. It keeps
Paperless as the system of record, reads and writes through the Paperless REST
API only, stores Archivist workflow state in PostgreSQL 18, and routes AI output
through validation, review, audit logging, and optional autopilot.

## Non-Negotiable Boundaries

Do not violate these rules:

- No direct writes to the Paperless database.
- No frontend calls to Paperless, Ollama, OpenAI, Anthropic, or compatible
  provider APIs.
- No plaintext secret values in settings responses, logs, audit metadata, tests,
  or docs.
- No unauthenticated or CSRF-free browser mutation endpoints.
- No settings, prompt, user, token, or provider-discovery action without a
  browser user session.
- No model output applied to Paperless without Rust-side validation.
- No autopilot behavior that bypasses validation.
- No public documentation that references private deployment hosts, private
  repos, private registry paths, or protected CI variable names.

When in doubt, keep the backend as the only integration boundary.

## Architecture Map

```text
React UI
  |
  | OpenAPI contract + handwritten API wrapper
  v
Rust Axum API  <---->  PostgreSQL 18
  |                        ^
  | Paperless REST         |
  | AI provider APIs       |
  v                        |
Rust Tokio worker ---------+
```

Key responsibilities:

| Area | Files |
| --- | --- |
| API routes, auth, orchestration | `crates/archivist-api/src/main.rs` |
| Worker jobs and apply flow | `crates/archivist-worker/src/main.rs` |
| Domain settings, roles, validation, prompts | `crates/archivist-core/src/lib.rs` |
| Database repositories | `crates/archivist-db/src/lib.rs` |
| SQL migrations | `migrations/*.sql` |
| Paperless REST client | `crates/archivist-paperless/src/lib.rs` |
| AI provider clients and parsers | `crates/archivist-ai/src/lib.rs` |
| Frontend app | `frontend/src/App.tsx` |
| Frontend API wrapper | `frontend/src/api/client.ts` |
| Generated OpenAPI types | `frontend/src/api/schema.ts` |
| OpenAPI source contract | `openapi/openapi.yaml` |
| Runtime docs | `docs/USER_GUIDE.md`, `docs/OPERATIONS.md` |

## Source Of Truth

Use this priority order:

1. Security boundaries in `docs/SECURITY_DESIGN.md`.
2. Current code and migrations.
3. OpenAPI contract in `openapi/openapi.yaml`.
4. Architecture and implementation design docs.
5. README and user-facing docs.

If behavior changes, update all affected layers in the same change:

- Rust handler/domain logic
- DB migration or repository code
- OpenAPI
- frontend wrapper/types/UI
- tests
- user/operator/agent docs

## Permissions And Sessions

Roles live in `archivist-core` and are enforced through API middleware.

Typical permission rules:

- `viewer`: read dashboard, runs, inventory
- `reviewer`: review queue and document chat
- `operator`: queue runs/batches and document chat
- `auditor`: audit log plus read-only operational views
- `admin`: all permissions

Browser sessions use HttpOnly cookies and CSRF protection. Unsafe browser
requests must include the CSRF token. API tokens are scoped, hashed, and should
not be allowed to perform session-only operations.

Session-only operations include:

- settings updates
- prompt management
- user and token management
- password/session operations
- document chat
- provider model discovery

## Settings And Secrets

Runtime settings are UI-managed. Secret values are submitted once and converted
to encrypted secret references.

Implementation rules:

- Accept optional secret payloads only in explicit settings endpoints.
- Resolve secrets in backend/worker code only.
- Never return secret values to the frontend.
- Redact secrets before storing JSON in audit metadata.
- Reuse existing settings fields for backward-compatible model/provider changes
  unless a migration is necessary.

## Paperless Integration

Paperless is accessed only through `archivist-paperless`.

Allowed:

- REST reads for documents, tags, correspondents, document types, custom fields
- REST patches for document metadata
- REST tag creation and tag updates

Forbidden:

- direct SQL writes to Paperless tables
- frontend calls to Paperless
- hidden apply paths outside the Paperless REST client

Apply logic must be idempotent where possible. Completion tags and trigger tag
cleanup should happen after successful stage completion.

## AI Providers

Provider clients live in `archivist-ai`.

Supported provider families:

- Ollama local
- Ollama Cloud
- OpenAI
- Anthropic
- OpenAI-compatible APIs

Rules:

- frontend never calls providers directly
- provider API keys are encrypted secret references
- model lists for local Ollama are loaded through the backend from `/api/tags`
- external providers should remain explicit operator choices
- provider errors should be user-readable without leaking tokens or request
  bodies

Local Ollama model discovery is intentionally a POST endpoint even though it is
read-only, because it triggers a backend network call and should be protected by
browser-session CSRF.

## Worker And Job Semantics

The worker processes PostgreSQL-backed jobs.

Preserve these properties:

- jobs are claimable with leases
- expired leases can be retried
- workers can restart without losing the queue
- apply operations avoid duplicate writes where practical
- every meaningful state transition is auditable
- model output is stored as an artifact before or alongside review/apply state

When changing worker behavior, inspect both API queue creation and worker
claim/apply logic.

## Review And Autopilot

Review mode:

- AI suggestions become review items
- users approve, reject, or edit suggestions
- approved changes are applied through Paperless REST

Autopilot mode:

- suggestions are validated in Rust
- valid patches can apply automatically
- invalid patches fail or fall back to review depending on settings

Never let raw model output directly mutate Paperless.

## Document Chat/RAG

Document Chat is a browser-session feature for reviewers, operators, and admins.

Flow:

1. User creates a chat session.
2. API searches local inventory candidates.
3. API fetches document content through Paperless REST.
4. Core logic builds bounded snippets.
5. Default text provider answers from snippets.
6. Messages, sources, provider/model metadata, and audit events are stored.

Security rules:

- no API-token chat access
- no direct frontend provider calls
- cap question length and document filters
- treat document text as untrusted evidence, not instructions
- cite sources as `[doc:<id>]`

Current retrieval is metadata/content based, not embedding backed.

## Frontend Patterns

The frontend is a compact operational app, not a marketing site.

Use existing patterns:

- `frontend/src/App.tsx` contains page components
- `frontend/src/api/client.ts` wraps API calls
- `frontend/src/api/schema.ts` is generated, do not hand-edit
- `frontend/src/styles/app.css` contains global styling
- use lucide icons already present in the app
- keep controls dense, predictable, and keyboard accessible

For new API fields:

1. Update `openapi/openapi.yaml`.
2. Run `pnpm --dir frontend generate:client`.
3. Update `frontend/src/api/client.ts`.
4. Update UI state and error handling.

## Database Patterns

Migrations are plain SQL in `migrations/`.

Rules:

- use PostgreSQL 18 features intentionally
- prefer `uuidv7()` for new primary keys
- keep migrations forward-only
- add indexes for new query patterns
- avoid storing raw secrets
- use `jsonb` for structured metadata when the shape can evolve

Repository functions live in `archivist-db`. Keep SQL concentrated there unless
there is already a local pattern in another crate.

## OpenAPI Contract

OpenAPI is the contract between backend and frontend. After editing it, run:

```bash
pnpm --dir frontend generate:client
```

Then inspect the generated schema enough to confirm the method, path, request
body, and response shape are correct.

## Documentation Rules

Keep docs useful for both humans and agents:

- explain the purpose before listing commands
- name the source files that own the behavior
- document safety boundaries next to workflows
- include failure modes and troubleshooting
- keep public docs free of private hosts, private repo paths, protected CI
  variables, credentials, and deployment-only secrets
- update README links when adding major docs

## Standard Verification

Run the smallest relevant checks while developing, then the full set before
hand-off.

Rust:

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo audit
cargo deny check
```

Frontend:

```bash
pnpm --dir frontend generate:client
pnpm --dir frontend typecheck
pnpm --dir frontend build
pnpm --dir frontend audit --audit-level high
```

Repo hygiene:

```bash
git diff --check
```

Public documentation safety scan:

```bash
PUBLIC_FORBIDDEN_PATTERN='<project-specific-public-safety-pattern>'
rg -uuu -n "$PUBLIC_FORBIDDEN_PATTERN" README.md docs openapi frontend/src crates migrations deploy
```

The public safety scan should return no hits for public-facing files.

## Safe Change Checklist

Before opening a PR or handing work back:

- [ ] Behavior matches README/docs and design constraints.
- [ ] Paperless writes go through REST only.
- [ ] Frontend talks only to Archivist.
- [ ] Settings/secrets are redacted.
- [ ] RBAC and session requirements are explicit.
- [ ] CSRF applies to browser writes and server-triggered network actions.
- [ ] Audit events are written for settings, security, review, apply, and chat
      actions where applicable.
- [ ] Worker changes remain resumable/idempotent.
- [ ] OpenAPI and generated frontend schema are in sync.
- [ ] Tests cover critical parsing, validation, permission, or retry logic.
- [ ] User-facing docs explain how to operate and troubleshoot the feature.

## Useful Entry Points For Agents

Start here for common tasks:

| Task | Start with |
| --- | --- |
| Understand product behavior | `README.md`, `docs/USER_GUIDE.md`, `docs/PROJECT_OVERVIEW.md` |
| Add or change API endpoint | `crates/archivist-api/src/main.rs`, `openapi/openapi.yaml` |
| Change runtime settings | `crates/archivist-core/src/lib.rs`, `crates/archivist-db/src/lib.rs`, Settings UI |
| Change provider logic | `crates/archivist-ai/src/lib.rs`, worker/API call sites |
| Change Paperless writes | `crates/archivist-paperless/src/lib.rs`, worker apply code |
| Change job behavior | `crates/archivist-worker/src/main.rs`, DB job functions |
| Change dashboard | DB stats functions, `frontend/src/App.tsx`, Recharts sections |
| Change Document Chat | chat routes, DB chat functions, core prompt helpers, Chat UI |
| Review security | `docs/SECURITY_DESIGN.md`, auth middleware, role permissions |

## Common Mistakes To Avoid

- Adding frontend calls to provider APIs for convenience.
- Introducing a new settings field when an existing field is sufficient.
- Updating OpenAPI but forgetting generated frontend types.
- Returning provider or Paperless error bodies that may include sensitive data.
- Treating model output as trusted JSON without validation.
- Adding `GET` endpoints that trigger backend network actions from browser UI.
- Forgetting audit events on security/settings/apply workflows.
- Writing docs that describe planned behavior as implemented behavior.
