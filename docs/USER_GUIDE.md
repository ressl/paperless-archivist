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

## Configure Paperless

Open `Settings` and configure:

- `Paperless Base URL`: the URL the Archivist API can reach.
  This must point to Paperless-ngx, not to the Paperless Archivist UI. In
  Docker Compose this is usually `http://paperless:8000`; in Kubernetes use the
  Paperless service DNS name that is reachable from the Archivist API pod.
- `Paperless API token`: a Paperless token for a user with permission to read
  documents and update metadata/tags.
- `Timeout`: how long backend/worker calls may wait for Paperless.
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

Sync reads Paperless metadata, tags, correspondents, document types, custom
fields, and document status through the REST API.

## Dashboard

Use the dashboard to understand operational state:

- total documents
- complete documents
- open OCR/tagging/title/field backlog
- failed jobs
- running jobs
- review load
- throughput and backlog charts
- job, run, and review status charts

Use the range selector to inspect recent or long-term trends. Backlog history
starts when dashboard snapshots are first recorded; older history is not
reconstructed.

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

In `review` mode, AI suggestions are not applied automatically.

1. Open `Review`.
2. Inspect the suggested patch.
3. Approve, reject, or edit the suggestion.
4. Approved changes are applied through the Paperless REST API.
5. The action writes an audit event.

For high-volume queues, select multiple review items and use `Approve selected`
or `Reject selected`. Batch review reports partial failures without hiding which
items failed.

Use review mode while tuning prompts, models, confidence thresholds, and tag
rules.

## Prompt Workbench

Archivist includes default prompts for:

- OCR and OCR post-processing
- tags
- title
- correspondent
- document type
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

## Autopilot Flow

Autopilot applies valid suggestions automatically. Enable it only after the
review results match your archive rules.

Autopilot still validates model output in Rust before applying changes. Invalid
or risky output can fall back to review depending on workflow settings.

Safe rollout pattern:

1. Run several batches in `review` mode.
2. Fix prompts/settings until suggestions are consistently good.
3. Enable autopilot for a small subset of documents.
4. Watch dashboard failures and audit events.
5. Expand only when the results are stable.

## Completion And Trigger Tags

Archivist uses workflow tags to coordinate with Paperless:

- trigger tags mark work that should run
- completion tags mark successful stages
- trigger tags are removed after the corresponding successful completion

Default completion tags include:

- `archivist:ocr-complete`
- `archivist:tagging-complete`
- `archivist:processed`

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
| Autopilot is too aggressive | Return to review mode and tighten validation/confidence thresholds |
| Chat has weak answers | Sync inventory, narrow to document IDs, improve document OCR quality, check default text model |

## Operating Principles

- Start in review mode.
- Keep Paperless as the source of truth.
- Do not bypass the Paperless REST API.
- Prefer local models for private archives.
- Treat external model providers as data processors.
- Keep prompts, settings, and audit events under change control.
