# Paperless Archivist Operations

Status: current runbook

## Runtime Components

Paperless Archivist runs as:

- `archivist-api`: Rust/Axum API plus static React UI
- `archivist-worker`: Rust/Tokio background worker
- PostgreSQL 18 database
- existing Paperless-ngx instance
- Ollama or another configured AI provider

PostgreSQL stores Archivist operational state only. Paperless-ngx remains the
document system of record.

## Required Environment Variables

API:

```text
DATABASE_URL=postgres://...
ARCHIVIST_SECRET_KEY=32+ byte deployment secret
ARCHIVIST_ADMIN_USERNAME=admin
ARCHIVIST_ADMIN_PASSWORD=initial admin password
ARCHIVIST_HTTP_ADDR=0.0.0.0:8080
ARCHIVIST_COOKIE_SECURE=true
ARCHIVIST_OIDC_ENABLED=false
ARCHIVIST_OIDC_ISSUER_URL=https://issuer.example.com
ARCHIVIST_OIDC_CLIENT_ID=paperless-archivist
ARCHIVIST_OIDC_CLIENT_SECRET=provider issued client secret
ARCHIVIST_OIDC_REDIRECT_URI=https://paperless-archivist.example.com/api/auth/oidc/callback
ARCHIVIST_OIDC_ADMIN_USERS=admin@example.com
ARCHIVIST_OIDC_DEFAULT_ROLES=viewer
ARCHIVIST_LOG_LEVEL=info
```

Worker:

```text
DATABASE_URL=postgres://...
ARCHIVIST_SECRET_KEY=same value as API
ARCHIVIST_WORKER_CONCURRENCY=2
ARCHIVIST_LOG_LEVEL=info
```

`ARCHIVIST_ADMIN_PASSWORD` is only used when no local users exist. If OIDC is
enabled, the API can start without a bootstrap password and will provision users
on successful OIDC login.

## Bootstrap

1. Start PostgreSQL 18.
2. Start the API with `ARCHIVIST_ADMIN_PASSWORD` or with OIDC enabled.
3. The API runs SQLx migrations and creates the bootstrap admin if needed.
4. Log in and replace the bootstrap password by creating named admin/operator
   accounts as appropriate, or log in with the OIDC admin allowlist account.
5. Configure Paperless and the default AI provider from the UI.
6. Start one or more worker replicas.

The service refuses an empty deployment unless a bootstrap admin password is
configured or OIDC is enabled.

## Docker Compose Startup

The Compose deployment starts the API/UI service, worker, and PostgreSQL 18:

```bash
cp deploy/compose/.env.example deploy/compose/.env
$EDITOR deploy/compose/.env
docker compose --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

Open `http://127.0.0.1:8080` and log in with the configured bootstrap admin.

To include a local Ollama service in the stack:

```bash
docker compose --profile ollama --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

Pull the local default models into the Ollama daemon before using local
inference:

```bash
ollama pull qwen3:8b
ollama pull qwen2.5vl:7b
```

Additional public-safe Compose profiles are documented in
[`deploy/compose/README.md`](../deploy/compose/README.md):

- minimal local PostgreSQL 18 stack
- local Ollama profile
- external PostgreSQL override
- Caddy reverse-proxy profile

The Compose services run with read-only root filesystems where practical,
dropped capabilities, private networking, health checks, and configurable
resource limits.

## Production Deployment Boundary

The public source repository ships application code, Docker Compose for local
operation, a generic Kubernetes package, and public CI. Private production
manifests, ingress hosts, runtime secrets, private registry image digests, and
deployment promotion state belong in a private deployment repository or
equivalent platform automation.

Use a managed PostgreSQL 18 instance or a separately operated PostgreSQL 18
cluster for production. The application schema is migrated by the API on
startup. Workers do not run migrations; they wait for the API-migrated schema
before claiming jobs.

For OIDC, configure `ARCHIVIST_OIDC_ENABLED`,
`ARCHIVIST_OIDC_ISSUER_URL`, `ARCHIVIST_OIDC_CLIENT_ID`,
`ARCHIVIST_OIDC_CLIENT_SECRET`, and `ARCHIVIST_OIDC_REDIRECT_URI` through your
runtime secret/configuration system.

ZITADEL applications must use Authorization Code flow with PKCE and the exact
redirect URI:

```text
https://paperless-archivist.example.com/api/auth/oidc/callback
```

There are two ways to grant roles, and they combine:

**1. IdP roles (recommended).** The app reads the roles your IdP asserts in the
ID token and maps them to its own roles. For ZITADEL this works out of the box
with the `archivist-<role>` convention — create project roles `archivist-admin`,
`archivist-operator`, `archivist-reviewer`, `archivist-auditor`,
`archivist-viewer` and grant them to users.

- The IdP **must assert roles into the ID token**. In ZITADEL: enable the
  project's *Assert Roles on Authentication* and make sure roles are included in
  the ID token (the *User Info inside ID Token* application setting). Without
  this the token carries no roles claim and role-based admin cannot work.
- `ARCHIVIST_OIDC_ROLES_CLAIM` (default `urn:zitadel:iam:org:project:roles`) is
  the claim read. The default also probes the project-scoped ZITADEL claim, so
  it usually needs no change. If a login still has no roles, the API logs a
  WARN listing the claim names the token actually carried — set this variable to
  the right one.
- `ARCHIVIST_OIDC_ROLE_MAPPINGS` (default
  `archivist-admin=admin,archivist-operator=operator,archivist-reviewer=reviewer,archivist-auditor=auditor,archivist-viewer=viewer`)
  maps IdP role strings to app roles. An IdP role with no mapping is ignored, so
  the IdP can never grant a role you did not map. IdP roles are **authoritative**:
  they replace the stored roles on every login (remove `archivist-admin` in the
  IdP and the next login drops admin).

**2. Admin allowlist (break-glass).** Set `ARCHIVIST_OIDC_ADMIN_USERS` to a
comma- or whitespace-separated list of trusted immutable `sub` values, usernames,
or verified email addresses. Matching users receive admin, operator, reviewer,
and auditor at login regardless of IdP roles — use it to bootstrap the first
admin or recover if IdP role assertion breaks. Prefer the immutable `sub` (it
keeps working even when a token omits `preferred_username`/verified email).

Users matched by neither source receive `ARCHIVIST_OIDC_DEFAULT_ROLES`
(default `viewer`).

For non-private Kubernetes users, start from
[`deploy/kubernetes/README.md`](../deploy/kubernetes/README.md). The package
contains API and worker Deployments, probes, resources, Service, Ingress,
NetworkPolicy, and secret references. Patch it with Kustomize or your GitOps
tooling; do not commit real secrets.

GA support, upgrade, rollback, and large-archive sizing are documented in:

- [`docs/STABILITY.md`](STABILITY.md)
- [`docs/MIGRATIONS.md`](MIGRATIONS.md)
- [`docs/PERFORMANCE.md`](PERFORMANCE.md)
- [`docs/RELEASE_CHECKLIST.md`](RELEASE_CHECKLIST.md)

## Paperless Configuration

Set in the UI:

- Paperless base URL, for example `http://paperless:8000`
- Paperless API token
- optional public URL

The API token is stored as a secret reference, not in normal settings. UI-entered
secret values are encrypted using `ARCHIVIST_SECRET_KEY`. Kubernetes deployments
can also use mounted secret files or environment secret references.

The integration uses Paperless REST endpoints only:

- list documents and metadata
- download originals for OCR
- patch document content and metadata
- create missing workflow tags

No direct Paperless database writes are used.

## Paperless Sync And Maintenance

Start with full sync until the initial inventory is complete. After that,
operators can enable delta sync in Settings. Delta sync calls the Paperless
documents endpoint with a modified-since filter and subtracts the configured
overlap window from the saved cursor. Keep a small overlap, for example five
minutes, so clock skew and slow Paperless writes do not skip recently modified
documents.

The active archive profile selects which Paperless connection the API and worker
use. The top-level Paperless URL and token remain the default profile. Additional
profiles are stored as runtime settings and are intended for controlled
multi-archive preparation; keep only one profile active in production until the
operator runbook and secrets are verified for that archive.

The dashboard maintenance panel has two safe Paperless tools:

- `Check consistency` compares Paperless documents with Archivist inventory and
  reports missing local rows, stale local rows, and mismatched title, tags,
  correspondent, document type, or document date.
- `Plan tag reconcile` performs a dry run for completion tags. It finds
  documents that already have all enabled stage completion tags but miss the full
  completion tag. `Apply planned tags` must be clicked separately and writes an
  audit event.

Use consistency check after bulk changes in Paperless, after changing the active
archive profile, after restoring backups, and before enabling full autopilot on a
large archive.

Custom-field mappings are managed in Settings as one line per field:

```text
Field name | enabled | alias one; alias two | instructions
```

Disabled fields are excluded from AI field extraction. Aliases and instructions
help the prompt match business terminology to Paperless custom field names while
reusing the existing settings schema.

## AI Provider Configuration

Ollama is the default provider. The UI also supports OpenAI, Anthropic, and
OpenAI-compatible endpoints. Set in the UI:

- default provider
- provider kind and base URL
- default text and vision models
- optional provider API key
- per-stage model overrides

Provider API keys are stored as encrypted secret references and are never sent to
the frontend after save. External providers should be opt-in because document
text may leave the local network.

Default providers are preconfigured so operators normally only enter API keys
for commercial providers:

| Provider | Text model | Vision/OCR model | Notes |
| --- | --- | --- | --- |
| Ollama | `qwen3:8b` | `qwen2.5vl:7b` | Local default. Pull both models before using local inference. |
| Ollama Cloud | `glm-5.1` | `qwen3-vl:235b-instruct` | Commercial Ollama API at `https://ollama.com`; enter an Ollama API key. |
| OpenAI | `gpt-5.5` | `gpt-5.5` | Enter the OpenAI API key, then select `openai` as default provider. |
| Anthropic | `claude-sonnet-4-6` | `claude-sonnet-4-6` | Enter the Anthropic API key, then select `anthropic` as default provider. |
| OpenAI-compatible | `qwen3:8b` | `qwen2.5vl:7b` | Disabled by default for local or gateway endpoints. |

The UI dropdowns contain the full app-compatible model catalog known at build
time for OpenAI, Anthropic, and Ollama Cloud. Local Ollama is different:
Archivist calls Ollama `/api/tags` from the backend, never from the browser,
and renders exactly the installed models sorted alphabetically. Each local
Ollama option shows:

```text
model name · parameter_size · quantization_level · size in GB
```

The Settings page has a refresh button for manual reloads. If Ollama is stopped,
times out, or returns an empty list, the UI shows an inline status and keeps the
stored settings value intact. If the stored model is not currently installed,
the dropdown keeps it as the selected value with a warning.

The local Ollama model dropdown includes a hardware recommendation tooltip. The
initial profile is stored in `frontend/src/hardwareRecommendations.json` so more
GPU profiles can be added without changing the component. Current NVIDIA
GeForce RTX 4060 Ti recommendations are `qwen3:4b-instruct` for text/LLM and
`glm-ocr` for vision/OCR.

## Notifications

Webhook notifications are configured in Settings. The webhook URL is stored as
an encrypted secret reference and can be tested from the UI. Operational worker
notifications are cooldown-limited and intentionally do not contain document
content, prompts, raw model output, API keys, or webhook URLs.

Supported events:

- review queue backlog at or above the configured threshold
- repeated recent processing failures at or above the configured threshold
- full autopilot configured but paused

Use the webhook receiver to route alerts to chat, incident tooling, email
bridges, or automation. The default payload includes `app`, `event`, `severity`,
`title`, `description`, and safe aggregate metadata only.

Current commercial Ollama Cloud models listed from `https://ollama.com/api/tags`
on 2026-05-13:

```text
cogito-2.1:671b
deepseek-v3.1:671b
deepseek-v3.2
deepseek-v4-flash
deepseek-v4-pro
devstral-2:123b
devstral-small-2:24b
gemini-3-flash-preview
gemma3:4b
gemma3:12b
gemma3:27b
gemma4:31b
glm-4.6
glm-4.7
glm-5
glm-5.1
gpt-oss:20b
gpt-oss:120b
kimi-k2:1t
kimi-k2.5
kimi-k2.6
kimi-k2-thinking
minimax-m2
minimax-m2.1
minimax-m2.5
minimax-m2.7
ministral-3:3b
ministral-3:8b
ministral-3:14b
mistral-large-3:675b
nemotron-3-nano:30b
nemotron-3-super
qwen3-coder-next
qwen3-coder:480b
qwen3-next:80b
qwen3-vl:235b
qwen3-vl:235b-instruct
qwen3.5:397b
rnj-1:8b
```

## Prompt Management

Prompts are versioned per stage and managed from the `Prompts` UI. Creating a
prompt creates a new immutable version; activating a version writes an audit
event. Worker AI artifacts reference the active prompt ID used for the job.
The Prompt Workbench shows the active prompt, older versions, prompt usage,
stage help, test output, and version comparison so changes can be validated
before activation.

Default prompts exist for:

- OCR
- OCR post-processing
- tags
- title
- correspondent
- document type
- document date
- custom fields

## Workflow Tags

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

The API and worker create missing workflow tags through the Paperless REST API.
Workers remove trigger tags and add completion tags after successful applies.

## Job Processing

Jobs are stored in PostgreSQL and leased with `FOR UPDATE SKIP LOCKED`.

Worker behavior:

- claims queued jobs in pipeline order
- increments attempts on lease
- retries transient failures with exponential backoff
- writes failed state after max attempts
- stores AI artifacts with redaction
- records provider, model, prompt version, normalized output, and duration
- extracts Paperless custom field values when custom fields exist
- writes audit events for job, review, and apply actions

Workers are safe to scale horizontally.

Job leases are coupled to the AI request timeout: the lease window granted at
claim and at every heartbeat bump is `max(300s, slowest enabled provider's
request_timeout_seconds + 60s)`. A long configured timeout (for example 600s
for slow local models) therefore never outlives the lease mid-call, which
would otherwise let a second worker replica reclaim and double-process the
job. Raising `request_timeout_seconds` automatically widens the lease — no
separate lease setting exists.

Metrics: `GET /metrics` (Prometheus text format) requires
`Authorization: Bearer <ARCHIVIST_METRICS_TOKEN>`; with the variable unset the
endpoint responds `503`. Configure the same token as `bearer_token` in the
Prometheus scrape job. The `paperless_archivist_audit_events` gauge is an
approximate row count from planner statistics.

Shutdown: the worker handles both SIGINT and SIGTERM (Kubernetes pod
termination). On signal it stops claiming new jobs and drains in-flight work
for up to 25 seconds so jobs finish terminally instead of expiring their
leases; keep `terminationGracePeriodSeconds` at 60 or higher for the worker
deployment (the bundled manifest sets 60). If work is still in flight at the
drain deadline the worker exits anyway and the lease reclaim takes over once
the lease window (see above) expires.

Every job log line includes a trace ID equal to the pipeline run ID, plus job
ID, document ID, stage, attempt, duration, and failure class. The Dashboard live
panel shows the same trace ID prefix for active jobs so operators can correlate
UI state, JSON logs, audit rows, and database records without exposing document
content.

## Metrics

Prometheus metrics are available without authentication:

```bash
curl http://127.0.0.1:8080/metrics
```

The service exports:

- queued/running/failed/succeeded jobs
- pending reviews, active runs, and audit event count
- automatic selector run count and queued-document count
- retry-scheduled count
- provider quota-exhausted event count (`paperless_archivist_provider_quota_total`)
- model-stage error count
- Paperless apply success/failure count
- Paperless apply latency count, sum, and p95

Recommended initial alert rules:

- `paperless_archivist_jobs_failed > 0` for 15 minutes
- `increase(paperless_archivist_job_retries_scheduled_total[30m]) > 10`
- `increase(paperless_archivist_provider_quota_total[1h]) > 0` — a provider hit
  its usage cap; the backlog is parked on a cooldown until it resets (#311)
- `paperless_archivist_model_errors_total > 0` for 15 minutes
- `paperless_archivist_apply_latency_ms_p95 > 10000` for 15 minutes
- `paperless_archivist_runs_active > 0` with no successful jobs for 60 minutes

Suggested SLOs for production archives:

- 99% of Paperless apply operations finish below 10 seconds.
- 95% of selected documents leave `queued` or `running` state within 30 minutes.
- Full-auto model-stage hard failure rate stays below 1% over 24 hours.

## Dashboard Analytics

The dashboard is served by:

```bash
curl 'http://127.0.0.1:8080/api/dashboard?range=30d'
```

Supported ranges are `24h`, `7d`, `30d`, `90d`, `12m`, and `all`.

The response contains:

- current backlog counts for OCR, tagging, failed, running, and complete
- KPI values for completion rate, open backlog, failure rate, review load,
  running jobs, and throughput
- range comparison deltas
- stacked stage status for OCR, title, document type, correspondent, tags, and
  fields
- throughput time series from job/run history
- backlog history from `dashboard_snapshots`
- job, run, and review status distributions
- quality metrics for review decisions, acceptance rate, edits, rejections,
  validation warnings, and uncertainty-routed reviews
- provider/model feedback counts and acceptance rates derived from review
  decisions

Dashboard snapshots are written opportunistically when the dashboard is queried
and the latest snapshot is older than five minutes. Fresh deployments therefore
show immediate current counts, while backlog history fills in over time after
migration `0004_dashboard_snapshots.sql`.

The frontend refreshes dashboard data every 30 seconds while the dashboard tab is
open.

The live processing panel separates hard failures from retryable transient
errors. A queued job with a previous `error_message` is shown as
`Retry Scheduled` or `Retry Ready` instead of marking the LLM or Paperless
service as failed. This is useful for Ollama runner crashes and temporary
Paperless timeouts: the operator can see the old error, the next retry time,
and whether the worker has resumed processing.

### Quality Evaluation

Use the quality strip and provider usage table to decide whether a model,
prompt, or workflow rule is improving production outcomes:

- a high approval rate with low edit/reject counts means suggestions are likely
  ready for broader automation
- rising uncertainty or validation-warning counts usually means the selected
  model, prompt, language profile, or confidence threshold needs attention
- provider/model feedback compares acceptance by model without exposing
  document content

Before rolling out prompt changes, run:

```bash
cargo test
```

The test suite includes golden document fixtures and prompt regression guards.
Update `docs/PROMPT_RELEASE_NOTES.md` whenever default prompt behavior changes.

## Security Governance

Security settings are managed in the Settings UI:

- audit retention days
- AI artifact retention days
- AI artifact storage mode: `redacted`, `metadata_only`, or `full`
- API token expiry requirement, default TTL, and maximum TTL

Use `redacted` for normal production operation. It preserves model/provider
metadata and usage information, but redacts document text, prompts, images, and
raw model text. Use `metadata_only` for stricter privacy. Use `full` only for a
short diagnostic window and apply retention afterwards.

The Audit page can verify the audit hash chain. New audit events include
`prev_event_hash` and `event_hash`; legacy events created before this feature
are reported separately. Retention can also be applied from the Audit page and
writes an `audit.retention_applied` event with deleted row counts.

API tokens are shown once, stored hashed, scoped, expiring by policy, and can be
rotated without keeping the old raw token. Rotation revokes the old token,
creates a new token with the same scopes, returns the raw replacement once, and
writes an `api_token.rotated` audit event.

## User and Session Operations

Admins manage local users, roles, password resets, sessions, and API tokens from
the `Users` UI. Disabling a user or resetting a password revokes that user's
active sessions and writes audit events.

Local passwords must be at least 12 characters. Ten failed login attempts lock
the account for 15 minutes; a successful login or admin password reset clears
the failed-attempt counter and lock.

## Review And Autopilot

`manual_review` mode:

- worker stores suggestions in `review_items`
- reviewer approves, rejects, or edits in the UI
- API applies approved patches to Paperless
- audit events record the decision and apply

`auto_select_review` mode:

- worker syncs Paperless inventory and queues documents with missing enabled stages
- suggestions still wait in `review_items`
- this is the recommended bridge between manual use and full automation

`full_auto` mode:

- worker syncs Paperless inventory and queues documents with missing enabled stages
- worker validates AI output in Rust
- valid suggestions are applied immediately
- validation failures fall back to review where configured

Autopilot never trusts model output directly. Full autopilot applies only a
validated Rust `DocumentPatch`.

Production rollout should keep automatic selection bounded:

1. Start in `manual_review` and process a representative sample.
2. Set include/exclude tags so the selector sees only the intended scope.
3. Enable `auto_select_review` with a small hourly or daily limit.
4. Watch Dashboard live status, recent retries/failures, Review debug context,
   and audit events.
5. Enable `full_auto` with `dry_run` so validated patches still land in Review.
6. Disable `dry_run` only after successful review of the same document class.
7. Keep a non-empty daily limit until the archive has had at least one stable
   processing cycle.

`Pause` is the emergency brake for automation. It stops selector and trigger
polling but does not delete queued jobs or revoke manual queue actions. The
worker records pause, resume, selector-run, dry-run review, and limit decisions
as audit events with redacted metadata.

## Language Operations

The worker records detected document language on `document_inventory` with a
confidence score and `document.language_detected` audit events when the stored
decision changes. Low-confidence or mixed-language documents remain usable but
should be reviewed before enabling `full_auto` broadly. `Tag output language` in
Settings affects only newly generated business tags; existing tags,
correspondents, document types, names, dates, and identifiers are not translated
automatically.

## Standard Metadata Operations

Correspondent and document type suggestions are matched against the latest
synced Paperless metadata cache. Run Paperless sync after adding or renaming
Paperless correspondents or document types. Document date extraction is local and
normalizes explicit issue/invoice/letter dates to `YYYY-MM-DD`; due, scan,
upload, and processing dates should remain in review unless operators lower the
date confidence threshold.

Overwrite settings are off by default. Keep them off while onboarding so
existing curated Paperless fields are not replaced. If overwrite is enabled,
review audit events and Paperless history after the first batch before enabling
`full_auto`.

## Document Chat

Document chat uses the same backend policy boundary as the rest of Archivist:

- users interact with the `Chat` UI
- the API retrieves candidates from `document_inventory`
- document content is fetched through the Paperless REST API
- prompts are sent through the configured default text provider
- chat sessions, messages, source snippets, and provider metadata are stored in
  PostgreSQL
- chat creation and message actions write audit events

Use the optional document ID filter when a user wants to constrain retrieval to
known Paperless documents. Without a filter, metadata similarity and source-term
matching determine the source set.

## OCR Runtime Dependency

OCR PDF rendering uses `pdftoppm` from Poppler in the container. The runtime
image installs `poppler-utils`. Image inputs (`png`, `jpg`, `jpeg`, `webp`) are
sent directly to the vision model.

PDF pages are rendered at a bounded 120 DPI before they are sent to a vision
model. This keeps local Ollama runners from receiving unnecessarily large page
images and makes GPU memory usage more predictable.

Rendered page images live in temporary directories and are deleted after each
job.

## PostgreSQL 18 Notes

The first migration fails on PostgreSQL versions below 18. The schema uses:

- `uuidv7()` defaults for Archivist IDs
- generated virtual columns for job priority
- JSONB artifacts and review patches
- `FOR UPDATE SKIP LOCKED` for job leasing
- `pg_trgm` for future fuzzy matching and diagnostics

Use SCRAM authentication and a dedicated non-superuser role in production.

## Backup and Restore

Back up PostgreSQL before large batch operations:

```bash
pg_dump "$DATABASE_URL" > archivist-backup.sql
```

Restore into a PostgreSQL 18 database:

```bash
psql "$DATABASE_URL" < archivist-backup.sql
```

Paperless documents are not stored in Archivist and must be backed up through the
Paperless-ngx backup process.

## Troubleshooting

Check readiness:

```bash
curl http://127.0.0.1:8080/readyz
curl http://127.0.0.1:8080/metrics
```

Common failures:

- `Paperless token is not configured`: save Paperless settings in the UI.
- `Ollama is reachable but model was not listed`: pull or configure the model.
- `Ollama vision returned 500`: inspect the live processing panel. If the job
  is `Retry Scheduled` or `Retry Ready`, the worker will retry it after backoff;
  repeated hard failures usually indicate a model/runtime issue or an input page
  the selected vision model cannot handle.
- stale `running` jobs: open Dashboard recovery tools, refresh candidates, then
  requeue stale leases. The endpoint is `POST /api/operations/recovery/stale-leases`.
- active runs without active jobs: use `POST /api/operations/recovery/stuck-runs`
  after inspecting the recovery candidates. The operation writes audit events.
- `pdftoppm exited`: verify Poppler is installed and the PDF is readable.
- repeated `validation` failures: use review mode and inspect AI output.
- empty dashboard history: query the dashboard after migration `0004`; snapshot
  history starts from the first post-migration dashboard request.

Audit events are available in the UI and in the `audit_events` table.
Auditors can also download recent events from `/api/audit/export.csv`.

## Release Hardening

The public CI path runs:

- Rust formatting, tests, Clippy, `cargo audit`, and `cargo deny`
- OpenAPI client generation drift check
- TypeScript typecheck and frontend build
- frontend dependency audit
- container image build

Before cutting a release:

1. Run the same checks locally.
2. Build the container with the release tag.
3. Render and validate your private production deployment package in staging.
4. Verify `/healthz`, `/readyz`, `/metrics`, login, Paperless test, provider
   test, sync, one OCR job, one tagging job, review apply, and audit export.
5. Promote the image by immutable tag or digest.
