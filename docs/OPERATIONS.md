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

## Production Deployment Boundary

The public source repository ships application code, Docker Compose for local
operation, and public CI. Production Kubernetes manifests, ingress hosts,
runtime secrets, private registry image digests, and deployment promotion state
belong in a private deployment repository or equivalent platform automation.

Use a managed PostgreSQL 18 instance or a separately operated PostgreSQL 18
cluster for production. The application schema is migrated by the API on
startup.

For OIDC, configure `ARCHIVIST_OIDC_ENABLED`,
`ARCHIVIST_OIDC_ISSUER_URL`, `ARCHIVIST_OIDC_CLIENT_ID`,
`ARCHIVIST_OIDC_CLIENT_SECRET`, and `ARCHIVIST_OIDC_REDIRECT_URI` through your
runtime secret/configuration system.

ZITADEL applications must use Authorization Code flow with PKCE and the exact
redirect URI:

```text
https://paperless-archivist.example.com/api/auth/oidc/callback
```

Set `ARCHIVIST_OIDC_ADMIN_USERS` to a comma- or whitespace-separated list of
trusted usernames or email addresses. Matching users receive the admin,
operator, reviewer, and auditor roles at login. Other new OIDC users receive
`ARCHIVIST_OIDC_DEFAULT_ROLES`, which defaults to `viewer`.

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

Default prompts exist for:

- OCR
- tags
- title
- correspondent
- document type
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
ai-fields
```

Default completion tags:

```text
ai-processed
ai-processed-ocr
ai-processed-tagging
ai-processed-title
ai-processed-correspondent
ai-processed-document-type
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

## Metrics

Prometheus metrics are available without authentication:

```bash
curl http://127.0.0.1:8080/metrics
```

The service exports queued/running/failed/succeeded jobs, pending reviews,
active runs, and audit event count.

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

Dashboard snapshots are written opportunistically when the dashboard is queried
and the latest snapshot is older than five minutes. Fresh deployments therefore
show immediate current counts, while backlog history fills in over time after
migration `0004_dashboard_snapshots.sql`.

The frontend refreshes dashboard data every 30 seconds while the dashboard tab is
open.

## User and Session Operations

Admins manage local users, roles, password resets, sessions, and API tokens from
the `Users` UI. Disabling a user or resetting a password revokes that user's
active sessions and writes audit events.

Local passwords must be at least 12 characters. Ten failed login attempts lock
the account for 15 minutes; a successful login or admin password reset clears
the failed-attempt counter and lock.

## Review and Autopilot

`review` mode:

- worker stores suggestions in `review_items`
- reviewer approves, rejects, or edits in the UI
- API applies approved patches to Paperless
- audit events record the decision and apply

`autopilot` mode:

- worker validates AI output in Rust
- valid suggestions are applied immediately
- validation failures fall back to review where configured

Autopilot never trusts model output directly.

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
