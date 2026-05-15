# Paperless Archivist User Guide

This guide explains how to use Paperless Archivist after it is installed. It is
written for operators, reviewers, and administrators who need to run a safe
document AI workflow around Paperless-ngx.

## What Archivist Does

Paperless Archivist connects to Paperless-ngx through the Paperless REST API and
adds an operated AI workflow:

- synchronize the Paperless document inventory
- run OCR, tagging, title, correspondent, document type, and field jobs
- review AI suggestions before they are applied
- enable autopilot only after validation is trusted
- add completion tags and remove trigger tags after successful work
- ask document questions through Document Chat/RAG
- audit settings, security, review, job, and apply actions

Archivist never writes directly to the Paperless database. Your browser also
never talks directly to Paperless, Ollama, OpenAI, Anthropic, or compatible
providers. The Archivist backend is the policy boundary.

## Roles

| Role | Typical use | Capabilities |
| --- | --- | --- |
| `viewer` | Read-only overview | Dashboard, runs, inventory |
| `reviewer` | Human review | Viewer capabilities, review queue, document chat |
| `operator` | Run processing | Viewer capabilities, queue runs/batches, document chat |
| `auditor` | Compliance review | Audit log, dashboard, runs |
| `admin` | Full administration | All features, users, settings, tokens |

Use named accounts for daily work. Keep the bootstrap admin account as a
break-glass account or replace it with named admin accounts after first setup.

## First Login

1. Open the Archivist URL, for example `http://127.0.0.1:8080`.
2. Log in with the configured bootstrap admin credentials or OIDC.
3. Open `Settings`.
4. Change the bootstrap password or create named admin/operator/reviewer users.
5. Configure Paperless and the model provider before queueing jobs.

## Interface Language

Archivist detects the browser language on first load and stores the selected UI
language in the browser. You can change it from the login screen, the sidebar,
or `Settings`.

English and German are complete UI catalogs. Other world languages are listed
with native names and marked as fallback languages. They keep locale-aware
number/date formatting but use the English UI text until a full translation
catalog is contributed.

UI language is independent from document language. Document language is detected
from OCR/content for prompt context. The language used for newly generated
Paperless business tags is configured separately as `Tag output language` in
`Settings`.

## Configure Paperless

Open `Settings` and configure:

- `Paperless Base URL`: the URL the Archivist API can reach.
  This must point to Paperless-ngx, not to the Paperless Archivist UI. In
  Docker Compose this is usually `http://paperless:8000`; in Kubernetes use the
  Paperless service DNS name that is reachable from the Archivist API pod.
- `Paperless API token`: a Paperless token for a user with permission to read
  documents and update metadata/tags.
- `Timeout`: how long backend/worker calls may wait for Paperless.
- `Delta sync`: optional. After a successful full sync, use modified-since
  polling with a configurable overlap window so later syncs scan only recently
  changed Paperless documents.
- `Active archive profile`: selects the Paperless archive profile used by the
  API and worker. The default profile uses the normal Paperless URL and token.
- `Allow Paperless-ngx login bridge`: optional. When enabled, users can log in
  with Paperless credentials. Archivist verifies the credentials through the
  Paperless token endpoint, does not store the Paperless password, and creates a
  separate `paperless-*` local account with the viewer role.

Then click `Test` in the Paperless section. The token is stored as an encrypted
secret reference; it is not returned to the frontend after saving.

The test button shows immediate feedback while it is running. After completion,
Settings displays whether the connection worked. If it fails, the result box
explains the likely cause and lists concrete checks such as token permissions,
Base URL, container DNS, proxy settings, and timeout.

## Configure Model Providers

Archivist ships with default provider records:

- `ollama`
- `ollama-cloud`
- `openai`
- `anthropic`
- `openai-compatible`

Commercial providers usually only need an API key. Local Ollama usually needs
installed models.

### Local Ollama

For local Ollama providers, the model dropdown is loaded from the configured
Ollama `/api/tags` response through the Archivist backend. The dropdown shows:

```text
model name · parameter_size · quantization_level · size in GB
```

Expected behavior:

- installed models are sorted alphabetically
- `Refresh` reloads the installed model list manually
- if Ollama is stopped or times out, Settings shows an inline error and keeps
  the saved value unchanged
- if the saved model is not installed, it remains selected with a warning
- the browser never calls Ollama directly

The info icon next to the local Ollama dropdown shows the current hardware
recommendation profile. The first profile is for NVIDIA GeForce RTX 4060 Ti and
currently recommends:

- Text / LLM: `qwen3:4b-instruct`
- Vision / OCR: `glm-ocr`

### Hosted Providers

For OpenAI, Anthropic, Ollama Cloud, and OpenAI-compatible endpoints:

1. Enter the API key in the provider card.
2. Select text and vision models from the dropdowns.
3. Save settings.
4. Test the provider.

Document text may leave your local network when external providers are enabled.
Only enable them when that matches your data handling policy.

The provider test also shows a running state and a result box. For local Ollama,
common hints include whether the service is reachable and whether the selected
model is installed. For commercial providers, the hints focus on API key,
provider Base URL, model access, and rate limits.

## Sync Inventory

The inventory is Archivist's local view of Paperless documents and stage status.
Run sync before processing:

1. Open `Dashboard`.
2. Click `Sync`.
3. Wait for the inventory to update.

Sync reads Paperless metadata, tags, correspondents, document types, the
Paperless document date, modified timestamp, custom fields, and document status
through the REST API.

Admins can also use the Paperless maintenance panel:

- `Check consistency` reports documents missing from local inventory, stale
  local inventory rows, and metadata mismatches between Paperless and Archivist.
- `Plan tag reconcile` performs a dry run for completion tags.
- `Apply planned tags` applies the planned full completion tag updates after the
  dry run has been reviewed.

Completion-tag reconcile only adds the full completion tag when all enabled
stage completion tags already exist on the Paperless document.

## Dashboard

Use the dashboard to understand operational state:

- total documents
- complete documents
- open OCR/tagging/title/correspondent/document type/document date/field backlog
- failed jobs
- running jobs
- review load
- throughput and backlog charts
- job, run, and review status charts

Use the range selector to inspect recent or long-term trends. Backlog history
starts when dashboard snapshots are first recorded; older history is not
reconstructed.

The live processing panel shows currently running jobs, recent model calls, and
recent retries or failures. `Retry Scheduled` means the worker already captured
the error and will try the job again after backoff; it is not the same as a hard
failed job.

The provider usage table shows provider/model/stage request counts, average and
P95 latency, token totals when the provider returns usage data, and estimated
cost when per-provider token pricing is configured in Settings.

## Run OCR And Tagging

From the dashboard or inventory:

- queue OCR for all documents missing OCR
- queue tagging for all documents missing tagging
- queue the configured full pipeline
- trigger selected stages for one document

Batch queues honor the workflow include/exclude tag rules configured in
Settings. Direct single-document triggers ignore those batch rules.

Jobs are stored in PostgreSQL and processed by the worker. They are designed to
be resumable and idempotent. If a worker restarts, another worker can pick up
expired leases.

## Review Flow

In `manual_review` and `auto_select_review` modes, AI suggestions are not
applied automatically.

1. Open `Review`.
2. Inspect the suggested patch. Standard metadata review items show current
   value, suggested value, confidence, evidence, and warnings.
3. Approve, reject, or edit the suggestion. For correspondent and document type
   reviews, edit the Paperless numeric ID if the reviewer needs a different
   existing value. For document date reviews, edit the ISO date directly.
4. Approved changes are applied through the Paperless REST API.
5. The action writes an audit event.

For high-volume queues, select multiple review items and use `Approve selected`
or `Reject selected`. Batch review reports partial failures without hiding which
items failed.

Use review mode while tuning prompts, models, confidence thresholds, and tag
rules.

## Standard Paperless Metadata

Archivist processes the normal Paperless fields separately from custom fields:

- `correspondent`: selected from synced Paperless correspondents.
- `document_type`: selected from synced Paperless document types.
- `document_date`: written to the Paperless `created` field as `YYYY-MM-DD`.

Existing non-empty values are protected by default. Admins can enable overwrite
for correspondent, document type, and document date independently in Settings.
The metadata confidence threshold controls correspondent/type suggestions; the
date confidence threshold controls document date extraction. Creation/proposal
toggles are conservative defaults for future controlled creation workflows and
should remain off unless the team has a review process for new Paperless values.

## Custom Field Mappings

Settings include custom-field mappings for Paperless custom fields. Use one line
per field:

```text
Field name | enabled | alias one; alias two | instructions
```

Set the second column to `disabled` to exclude a field from AI extraction.
Aliases and instructions help the prompt map business terminology to the exact
Paperless custom field name while keeping the existing settings schema.

## Prompt Workbench

Archivist includes default prompts for:

- OCR and OCR post-processing
- tags
- title
- correspondent
- document type
- document date
- custom fields

The defaults are versioned database records. They are strict by design:
classification prompts use exact Paperless metadata names, OCR prompts return
plain text, and structured stages return JSON for backend validation. See
[Prompt Pack](PROMPTS.md) for details.

Use `Prompts` to inspect, edit, compare, and test prompt content before
activating it:

1. Select the stage in the left pipeline list.
2. Review the active prompt and its version history.
3. Hover or focus the info icon for the stage purpose, expected output, safety
   rules, and examples.
4. Edit the prompt content. Saving creates a new immutable version; it never
   overwrites older versions.
5. Compare the editor content with another version when tuning changes.
6. Provide sample text or a Paperless document ID and click `Test Current
   Editor`.
7. Review raw model output, parsed output, validation errors, warnings,
   provider/model, and duration.
8. Activate the version only after the test result matches your archive rules.

Prompt tests call the configured model provider and write audit events, but they
never apply changes to Paperless.

## Workflow Modes

Archivist has three processing modes:

| Mode | API value | What happens |
| --- | --- | --- |
| Manual trigger + review | `manual_review` | Documents run only when explicitly queued or marked with a trigger tag, and suggestions wait for review. |
| Autopilot selector + review | `auto_select_review` | Archivist automatically queues documents with missing enabled stages, and suggestions wait for review. |
| Full autopilot | `full_auto` | Archivist automatically queues documents and applies validated suggestions to Paperless. |

Full autopilot still validates model output in Rust before applying changes.
Invalid or risky output can fall back to review depending on workflow settings.

The Dashboard includes operational controls for administrators:

- `Pause` stops automatic selector and trigger polling. Manual queue buttons can
  still be used for controlled tests.
- `Dry-run` lets `full_auto` select and process documents, but validated results
  are written to Review instead of Paperless.
- `Hourly limit` and `Daily limit` cap automatic document selection. Empty values
  mean no limit.
- The live status cards show whether the selector, Paperless, or model provider
  is idle, running, paused, limited, retrying, or failing.

Safe rollout pattern:

1. Run several batches in `manual_review` mode.
2. Fix prompts/settings until suggestions are consistently good.
3. Enable a low hourly/daily limit and switch to `auto_select_review`.
4. Watch dashboard failures, Review debug context, and audit events.
5. Enable `full_auto` with `Dry-run` first.
6. Remove dry-run only when the results are stable.

Inventory and Review expose a compact debug context for each document. Use it to
see why the selector did or did not pick a document, which language was detected
for prompt context, which tag output language is configured, and whether a run
is already queued, waiting for review, or blocked by tags.

## Language And Tag Output

Archivist detects the document language from OCR or existing Paperless content
and stores the result as a BCP-47 language tag with confidence. The inventory
table shows the current language decision so low-confidence or mixed-language
documents are visible during review/debug work.

In `Settings` -> `Workflow`, `Tag output language` controls the language used
for newly generated business tags. The selector uses standard ISO language tags
and browser-native language names. Existing Paperless tags are not translated or
renamed automatically; the model must return existing allowed tags exactly as
listed.

## Completion And Trigger Tags

Archivist uses workflow tags to coordinate with Paperless:

- trigger tags mark work that should run
- completion tags mark successful stages
- trigger tags are removed after the corresponding successful completion

Default completion tags include:

- `archivist-ocr`
- `archivist-tags`
- `ai-processed`

Workflow tag names are configurable in `Settings`.

## Document Chat/RAG

Document Chat lets reviewers, operators, and admins ask questions against
retrieved Paperless document content.

1. Open `Chat`.
2. Create a chat session.
3. Ask a question.
4. Optionally restrict retrieval to specific Paperless document IDs.
5. Review citations in the answer, for example `[doc:123]`.

Archivist retrieves candidate documents from the local inventory, fetches
document content through Paperless REST, builds bounded snippets, and sends only
those snippets to the configured default text provider.

Current retrieval is metadata/content based. Embedding-backed retrieval is a
future improvement.

## Users, Sessions, And API Tokens

Admins can manage:

- users and roles
- local passwords
- browser sessions
- scoped API tokens

API tokens are intended for automation. They are stored hashed and only shown
once at creation time. Security-sensitive operations such as settings changes
and model discovery require an interactive browser session.

## Audit Log

Use `Audit` to inspect important actions:

- login/security changes
- settings changes
- prompt activation
- job and batch operations
- review decisions
- Paperless apply operations
- document chat events

Secrets are redacted before they are stored in audit metadata.
Use `Export CSV` when an auditor needs the recent audit trail outside the UI.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Paperless test fails | Base URL, token permissions, network path from Archivist API to Paperless |
| Ollama models do not load | Ollama service status, provider base URL, timeout, API key if using a protected endpoint |
| Saved model says not installed | Pull/install the model in Ollama or select one of the installed dropdown entries |
| Jobs stay queued | Worker process, database connectivity, worker logs |
| Jobs fail repeatedly | Provider test, Paperless token permissions, document file type, prompt output shape |
| Review queue grows | Switch batches to smaller scope, add reviewers, improve prompts/confidence settings |
| Autopilot is too aggressive | Return to `manual_review` or `auto_select_review` and tighten validation/confidence thresholds |
| Chat has weak answers | Sync inventory, narrow to document IDs, improve document OCR quality, check default text model |

## Operating Principles

- Start in review mode.
- Keep Paperless as the source of truth.
- Do not bypass the Paperless REST API.
- Prefer local models for private archives.
- Treat external model providers as data processors.
- Keep prompts, settings, and audit events under change control.
