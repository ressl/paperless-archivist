# Paperless Archivist API Reference

Status: implemented API surface for the current product branch
Contract: [`openapi/openapi.yaml`](../openapi/openapi.yaml)

Paperless Archivist exposes one HTTP API from the Rust/Axum service. The React
frontend talks only to `/api/*`; Paperless-ngx, Ollama, OpenAI, Anthropic, and
other providers are reached exclusively from backend or worker code.

## Authentication

Browser users authenticate with either local credentials or OIDC:

- `POST /api/auth/login`
- `GET /api/auth/oidc/config`
- `GET /api/auth/oidc/login`
- `GET /api/auth/oidc/callback`
- server-side session storage
- HttpOnly session cookie
- CSRF token returned in the login response and sent as `X-CSRF-Token` on unsafe
  browser requests

Automation can use API tokens:

- `Authorization: Bearer <token>`
- tokens are stored hashed
- scopes are checked against the same internal permission model as roles
- mutable user, token, prompt, and settings operations still require an
  interactive user session for defense-in-depth

Error responses use JSON:

```json
{ "error": "message" }
```

Common status codes:

- `400`: invalid JSON or invalid request shape
- `401`: missing or invalid authentication
- `403`: authenticated principal lacks permission
- `500`: backend, database, Paperless, or provider failure

## Public Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/healthz` | Process liveness check. |
| `GET` | `/readyz` | Database readiness check. |
| `POST` | `/api/auth/login` | Create a browser session. |
| `POST` | `/api/auth/paperless-login` | Create a browser session through the optional Paperless login bridge. |
| `GET` | `/api/auth/oidc/config` | Return whether SSO is enabled and the login URL. |
| `GET` | `/api/auth/oidc/login` | Start Authorization Code + PKCE and redirect to the OIDC provider. |
| `GET` | `/api/auth/oidc/callback` | Validate the OIDC response and create a browser session. |

`GET /metrics` is an operator integration endpoint, not an unauthenticated
public endpoint. It accepts only the dedicated bearer value configured through
`ARCHIVIST_METRICS_TOKEN`: an unset token returns `503`, a missing or invalid
token returns `401`, and a valid token returns the Prometheus exposition with
`200`. Normal Archivist API bearer tokens are not accepted.

## Auth And Sessions

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/auth/me` | Return the current user, roles, and CSRF token. |
| `POST` | `/api/auth/logout` | Revoke the current browser session. |
| `POST` | `/api/auth/change-password` | Change own password and revoke other sessions. |
| `GET` | `/api/auth/sessions` | List own browser sessions, or all sessions as an administrator. Requires an interactive cookie session; API tokens are rejected regardless of scope. |
| `POST` | `/api/auth/sessions/{id}/revoke` | Revoke a browser session as an administrator. Requires an interactive cookie session; API tokens are rejected regardless of scope. |

## Runtime Settings

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/settings` | Read Paperless, AI, workflow, OCR, tagging, and field settings. |
| `PUT` | `/api/settings` | Save runtime settings plus optional Paperless/provider secret values. |
| `POST` | `/api/settings/test-paperless` | Test the configured Paperless REST connection. |
| `POST` | `/api/notifications/test` | Send a safe webhook test payload using the saved notification settings. |
| `POST` | `/api/model-providers/test` | Test the selected default model provider. |
| `POST` | `/api/model-providers/{name}/models` | List installed models for a configured Ollama provider. |
| `PUT` | `/api/workflow/mode` | Change between manual review, auto-select with review, and full auto. |
| `PATCH` | `/api/workflow/controls` | Update automation pause, dry-run, and hourly/daily limit controls. |
| `GET` | `/api/secret-references` | List secret references without returning secret values. |

`PUT /api/settings` accepts a `RuntimeSettings` object plus optional secret
payloads: `paperless_token`, `provider_secrets`, and
`notification_webhook_url`. Secret values are encrypted into secret references
and are not returned to the frontend after saving.

Settings updates require an interactive administrator cookie session and the
matching `X-CSRF-Token` value. Bearer/API-token authentication is rejected
regardless of token scope. A session-aware client must first read the current
settings, preserve unrelated fields, and send the complete update document.

Default provider records are created for:

- `ollama`
- `ollama-cloud`
- `openai`
- `anthropic`
- `openai-compatible`

Commercial providers require only their API key unless an operator intentionally
changes the model or base URL.

`PATCH /api/workflow/controls` accepts any subset of:

```json
{
  "paused": true,
  "dry_run": true,
  "hourly_document_limit": 10,
  "daily_document_limit": null
}
```

The route requires settings write permission and an interactive user session.
It writes both the normal settings audit event and a workflow-specific audit
event for pause, resume, dry-run, or limit changes.

`POST /api/model-providers/{name}/models` is read-only but intentionally uses
POST so browser-session CSRF protection applies before the backend makes an
outbound provider request. It requires the same settings read permission as the
Settings page plus a browser user session, resolves the saved provider
configuration server-side, and calls Ollama `/api/tags` with a short timeout.
The browser never contacts Ollama directly.

## Prompts

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/prompts` | List prompt versions per stage. |
| `GET` | `/api/prompts/usage` | Return read-only usage counts and last model/provider per prompt version. |
| `POST` | `/api/prompts` | Create a new immutable prompt version. |
| `POST` | `/api/prompts/test` | Test a prompt against sample text or a Paperless document without applying changes. |
| `POST` | `/api/prompts/{id}/activate` | Activate a prompt version for its stage. |

Prompt stages are `ocr`, `ocr_fix`, `title`, `document_type`, `correspondent`,
`document_date`, `tags`, and `fields`. Creating or activating prompts writes
audit events.

`POST /api/prompts/test` calls the configured text provider, parses the output
for the selected stage, runs Rust-side validation, returns raw and parsed
output, and writes a `prompt.tested` audit event. It never patches Paperless.

## Paperless Inventory And Jobs

`GET /api/dashboard/live` returns lightweight processing debug status:
`selector`, `llm`, `paperless`, active runs/jobs, recent retries/failures,
workflow safety state, and the next selector scan estimate. It is intentionally
status-only and does not expose document content or provider secrets.
Active runs and jobs include `trace_id`, which is the pipeline run UUID used in
worker JSON logs and audit correlation.

Inventory and Review responses include an optional `debug_context` object with
selector reason, workflow mode, pause/dry-run state, detected prompt language,
tag output language, run status, next required stage, and last error. The field
is for UI/debugging support and remains safe to display to authenticated users
with the existing inventory/review permissions.

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/api/paperless/sync-metadata` | Synchronize document metadata, tags, correspondents, document types, document dates, modified timestamps, and custom fields from Paperless. Uses configured delta sync when enabled. |
| `GET` | `/api/paperless/consistency` | Compare the Paperless document list with Archivist inventory and report missing local rows, stale local rows, and metadata mismatches. |
| `POST` | `/api/paperless/completion-tags/reconcile` | Dry-run or apply completion-tag reconciliation for documents that miss the full completion tag and either have all enabled stage tags or terminal local status (`succeeded`, `skipped`, `not_needed`, `rejected`) for every enabled stage. Status-based writes are rechecked under the per-document run lock; active or review-waiting runs are excluded. An optional `document_ids` list pins apply to a prior dry-run plan. |
| `GET` | `/api/inventory?limit=100&offset=0` | List the local document inventory and per-stage status. |
| `POST` | `/api/documents/{paperless_document_id}/trigger` | Queue selected stages for one Paperless document. |
| `POST` | `/api/batches/ocr` | Queue OCR for documents missing OCR. |
| `POST` | `/api/batches/tags` | Queue tagging for documents missing tagging. |
| `POST` | `/api/batches/full` | Queue the configured full pipeline for open documents. |

## Operations Recovery

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/operations/recovery?older_than_seconds=600` | List stale leases and active runs that have no active jobs. |
| `POST` | `/api/operations/recovery/stale-leases` | Requeue expired running jobs and clear their leases. |
| `POST` | `/api/operations/recovery/stuck-runs` | Complete all-succeeded stuck runs or fail active runs that no longer have active jobs. |

Recovery writes audit events and requires run write permission plus a browser
user session for mutable actions. Request body:

```json
{ "older_than_seconds": 600 }
```

The value is clamped to 60 seconds through 24 hours.

Single-document trigger body:

```json
{
  "stages": ["ocr", "tags"],
  "mode": "manual_review"
}
```

When fields are omitted, the API uses the configured workflow stages and
processing mode. Valid processing modes are `manual_review`,
`auto_select_review`, and `full_auto`. The legacy request values `review` and
`autopilot` are accepted for backward compatibility.

Inventory items include language debug fields when detection has run:

```json
{
  "detected_language": "de",
  "detected_language_confidence": 0.92,
  "detected_language_source": "heuristic"
}
```

Runtime settings include `tagging.tag_output_language`, a BCP-47 language tag
used for newly generated business tags. Existing Paperless tags are still
returned exactly as configured.

Paperless sync returns a summary that includes `archive`, `mode`,
`delta_cursor`, and synced object counts. `mode` is one of `full`,
`full_initial`, `delta`, or `full_after_delta_error`. The delta cursor is based
on the sync start time and the configured overlap window, so interrupted or slow
syncs can be retried without skipping recently changed documents.

Completion-tag reconciliation defaults to dry run:

```json
{ "dry_run": true }
```

The response contains `planned` document IDs and tag additions. Send
`{"dry_run": false}` only after reviewing the plan. Reconciliation writes an
audit event and updates Paperless only through the Paperless REST API.

## Dashboard

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/dashboard?range=30d` | Return backlog counts, analytics series, and provider usage. |

Supported ranges are:

- `24h`
- `7d`
- `30d`
- `90d`
- `12m`
- `all`

Response shape:

```json
{
  "counts": {
    "total_documents": 0,
    "complete": 0,
    "missing_ocr": 0,
    "missing_tagging": 0,
    "missing_title": 0,
    "missing_correspondent": 0,
    "missing_document_type": 0,
    "missing_document_date": 0,
    "missing_fields": 0,
    "waiting_review": 0,
    "failed": 0,
    "running": 0,
    "never_processed": 0
  },
  "stats": {
    "generated_at": "2026-05-13T10:00:00Z",
    "selected_range": "30d",
    "available_ranges": [{ "key": "30d", "label": "30 days" }],
    "kpis": {
      "completion_rate": 0.0,
      "open_backlog": 0,
      "failure_rate": 0.0,
      "review_load": 0,
      "running_jobs": 0,
      "throughput": 0
    },
    "comparison": {
      "jobs_created_delta": 0,
      "jobs_succeeded_delta": 0,
      "jobs_failed_delta": 0,
      "open_backlog_delta": 0
    },
    "stage_status": [],
    "throughput_series": [],
    "backlog_series": [],
    "job_status": [],
    "run_status": [],
    "review_status": [],
    "provider_usage": [],
    "quality": {
      "review_decisions": 0,
      "review_approved": 0,
      "review_edited": 0,
      "review_rejected": 0,
      "acceptance_rate": null,
      "uncertainty_reviews": 0,
      "validation_warning_reviews": 0
    }
  }
}
```

The frontend renders these values as KPI tiles, stacked stage status, throughput
series, backlog history, status distribution charts, and provider
usage/token/cost/latency/feedback tables. The quality object summarizes review
acceptance, edits, rejections, and uncertainty routing for the selected range.
Cost estimates use optional per-provider pricing fields in runtime settings.

## Document Chat

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/chat/sessions` | List document chat sessions visible to the current user. |
| `POST` | `/api/chat/sessions` | Create a document chat session. |
| `GET` | `/api/chat/sessions/{id}` | List messages and stored sources for a chat session. |
| `POST` | `/api/chat/sessions/{id}/messages` | Ask a question and store the assistant response. |

Message body:

```json
{
  "question": "Which invoices mention ACME?",
  "document_ids": [12, 98],
  "max_sources": 6
}
```

`document_ids` is optional. Without it, Archivist searches the local inventory,
fetches candidate document content through the Paperless REST API, builds bounded
source snippets, and calls the configured default text provider. The response
contains the answer plus stored source snippets:

```json
{
  "answer": "ACME appears in invoice 12 [doc:12].",
  "sources": [
    {
      "paperless_document_id": 12,
      "title": "ACME invoice",
      "snippet": "Invoice from ACME ...",
      "score": 1.0,
      "source_kind": "paperless_content"
    }
  ]
}
```

Chat requires an authenticated browser session with a role that has chat
permission. Chat session creation and messages write audit events.

## Review Queue

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/reviews?status=pending&limit=100` | List review items. |
| `POST` | `/api/reviews/{id}/approve` | Apply the worker suggestion. |
| `POST` | `/api/reviews/{id}/reject` | Reject the suggestion. |
| `POST` | `/api/reviews/{id}/edit` | Apply a reviewer-edited patch. |
| `POST` | `/api/reviews/batch` | Approve/apply or reject up to 100 review items. |

Edit body:

```json
{
  "patch": {
    "title": "Corrected title",
    "correspondent": 12,
    "document_type": 4,
    "created": "2026-04-12",
    "tags": [10, 42]
  }
}
```

Apply actions write to Paperless through the Paperless REST API, update local run
state, adjust workflow tags, and write audit events. Patch bodies use Paperless
numeric IDs for `correspondent`, `document_type`, and `tags`; the Paperless
document date is `created` in ISO `YYYY-MM-DD` format.

Batch review returns per-item failures for partial failures and writes a
`review.batch_approve` or `review.batch_reject` audit event.

## Audit, Users, And Tokens

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/audit` | List audit events. |
| `GET` | `/api/audit/export.csv` | Export recent audit events as CSV. |
| `GET` | `/api/audit/integrity` | Verify the audit hash chain. |
| `POST` | `/api/audit/retention/apply` | Apply configured audit and AI-artifact retention. |
| `GET` | `/api/users` | List local users. |
| `POST` | `/api/users` | Create a user. Username and non-empty email share a trimmed, case-insensitive identity namespace; conflicts return `409`. |
| `POST` | `/api/users/{id}/enable` | Enable a user. |
| `POST` | `/api/users/{id}/disable` | Disable a user and revoke sessions. |
| `POST` | `/api/users/{id}/roles` | Replace user roles. |
| `POST` | `/api/users/{id}/reset-password` | Reset password and revoke sessions. |
| `GET` | `/api/api-tokens` | List API tokens. |
| `POST` | `/api/api-tokens` | Create a token and return the raw token once. |
| `POST` | `/api/api-tokens/{id}/rotate` | Revoke an existing token and return a raw replacement once. |
| `DELETE` | `/api/api-tokens/{id}` | Revoke an API token. |

Supported API token scopes are `runs:read`, `runs:write`, `inventory:read`,
`batches:write`, `chat:write`, `reviews:read`, `reviews:write`, `settings:read`,
`settings:write`, `users:manage`, and `audit:read`.

Token creation accepts `expires_in_days`. When omitted, the configured default
TTL is used. Rotation accepts the same field and preserves the original scopes.
Retention and token policy are part of runtime settings under `security`.

Audit CSV exports include `prev_event_hash` and `event_hash` for events created
after hash-chain tracking was enabled. `GET /api/audit/integrity` returns
whether the current chain verifies, how many hashed events were checked, and how
many legacy events predate hash-chain tracking. `legacy_precision_events`
counts pre-v1.17 hashes that verify only after reconstructing the three
sub-microsecond timestamp digits discarded by PostgreSQL. Verification tries
the finite 999 non-canonical suffixes once, off the async executor and under
process- plus database-wide single-flight locks. It persists the matching
suffix as a lookup hint that is validated against the hash on every later
scan. Stored events and hashes are never rewritten, and changes to every hashed
payload field still fail verification.

Roles are:

- `admin`
- `operator`
- `reviewer`
- `auditor`

## OpenAPI Workflow

The source contract is [`openapi/openapi.yaml`](../openapi/openapi.yaml). When
the backend route surface or schemas change:

1. Update the Rust handlers and domain types.
2. Update `openapi/openapi.yaml`.
3. Regenerate the frontend client from `frontend/`:

   ```bash
   pnpm generate:client
   ```

4. Run:

   ```bash
   cargo test --workspace
   pnpm typecheck
   pnpm build
   ```
