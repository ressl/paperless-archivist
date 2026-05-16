# Release Notes

> Versioning policy: the Git tag (`vX.Y.Z`) is the source of truth.
> `frontend/package.json` tracks the UI release alongside the tag (currently
> `1.5.4`). The Rust workspace `Cargo.toml` files remain at the pre-GA
> internal version `0.3.2`; bumping them does not change the release.

## v1.5.4 — Full Auto really completes every document

Closes the gap between what `workflow.mode = full_auto` promised and what was
actually shipping to Paperless. Four changes ship together:

### 1. Backfill the consolidated `metadata` stage onto OCR-only pipeline runs

Historical `pipeline_runs` queued by trigger polling against documents tagged
only with the OCR trigger were created with `stages = ["ocr"]` — they
terminated after OCR with no `Title` / `Correspondent` / `Tags` /
`DocumentType` / `Date` suggestions ever produced. The Review queue filled up
with content-only review items that the operator could not meaningfully act
on. On worker startup the new
`archivist_db::backfill_metadata_stage_for_ocr_only_runs` (idempotent,
single-transaction) finds every such run, appends `metadata` to its `stages`
array, inserts a queued `metadata` job behind the existing OCR job (using
`stage_priority = 20` so it sequences after OCR), and flips already-finished
runs back to `queued` so the worker re-picks them up. After the first
successful pass, subsequent startups find nothing to do.

### 2. Autopilot review drain runs off the main tick loop

`drain_pending_reviews_if_autopilot_tick` used to be `.await`-ed on the
worker's 5-second tick loop. A drain of 100 items at ~5s of Paperless API
latency each took ~8 minutes, during which OCR job processing was completely
starved. The drain is now spawned via `tokio::spawn` with an atomic re-entry
guard (mirroring the trigger-polling pattern), so the main loop keeps
claiming and processing OCR jobs in parallel.

`PER_TICK_CEILING` is also bumped from 100 → 500, and the outer drain
timeout from 8 → 30 minutes to match. Sustained throughput against a 2515-
item backlog moves from ~140 items/hour to roughly an order of magnitude
higher, bounded only by Paperless API write latency.

### 3. Real Paperless title in the Review queue

`list_reviews` now joins `document_inventory.title` and surfaces it as
`paperless_title` on every `ReviewItem`. The Review card prefers it over the
generic `Document {id}` fallback (which it falls back to only when the
inventory has no cached title for the document yet).

### 4. Selector pill no longer renders literal `Selector unknown`

When the server-side `debug_context` did not include `selector_reason` or
`next_required_stage` — the common case for review items, since those fields
describe the auto-selector decision, not why a review exists — the frontend
fell back to the literal English word "unknown" embedded inside the
translated "Prompt-Sprache de; Tag-Sprache de; Selector …" pill. The pill
now uses a separate i18n template (`review.debug_summary_no_selector` /
`inventory.debug_summary_no_selector`) that omits the selector segment when
no meaningful value is available, and the corresponding `<dl>` row is
hidden too. New keys added to all seven UI locales.

### Heads up for operators on upgrade

* The backfill runs once on the first v1.5.4 worker boot. Expect a one-line
  `metadata-stage backfill lifted OCR-only pipeline_runs to include the
  metadata stage` log entry with `runs_updated` and `jobs_inserted` counts.
* Documents whose OCR was already `succeeded` will have their run flipped
  back to `queued` so the worker can pick up the new metadata job — the
  dashboard "succeeded" badge for those documents will briefly drop and then
  climb back as metadata runs.
* No application behaviour change for fresh installs. UI sidebar reads
  `v1.5.4`.

## v1.5.3 — Apply Debian Security patches in the runtime image

The runtime stage of `Dockerfile` now runs `apt-get upgrade` so every build
pulls the current Debian Security patches for libraries that ship
pre-installed in `debian:bookworm-slim` and are otherwise frozen at whatever
version Docker Hub baked into the base tag.

Without this, image scans (Trivy) flagged CVEs that Debian had already fixed
upstream — observed examples: CVE-2026-0861 (`libc-bin` / `libc6`),
CVE-2026-4878 (`libcap2`), CVE-2026-29111 (`libsystemd0` / `libudev1`). The
patched versions were available in the Debian Security mirror; we just
weren't pulling them.

The fix is a one-line addition to the runtime layer:

```dockerfile
RUN apt-get update \
  && apt-get -y --no-install-recommends upgrade \
  && apt-get install -y --no-install-recommends ca-certificates curl poppler-utils \
  && rm -rf /var/lib/apt/lists/*
```

Multi-stage build stages (`rust:1.95-bookworm`, `node:26-bookworm`) are
unchanged — only the compiled binaries and the frontend dist are copied into
the runtime image, so their base libraries never ship.

No application behaviour changes. UI sidebar reads `v1.5.3`.

## v1.5.2 — Pipeline-run + tag-resolution fixes

Two surgical fixes on top of v1.5.1:

- `queue_full_batch` now queues a single full pipeline run per document
  covering all enabled stages (`["ocr", "metadata"]`) instead of N
  single-stage runs.
- Tag names and custom-field names emitted by the metadata stage are
  resolved to integer IDs before being stored in `review_items`, so
  `POST /api/reviews/{id}/approve` no longer 500s with
  `invalid type: string "<name>", expected i32`.

## v1.5.1 — Root-cause fix for glm-ocr GGML_ASSERT crashes

Pins Ollama's `options.num_ctx` on vision and text calls so the configured
primary vision model (glm-ocr by default) stops crashing on realistic
document pages. This is the **root-cause** fix for the GGML_ASSERT runtime
crash that the v1.5.0 fallback machinery had to paper over.

### What was crashing

Vision runs against `glm-ocr` (or any vision model that expands a page into
many thousands of vision tokens) were aborting Ollama's llama runner with:

```
GGML_ASSERT(a->ne[2] * 4 == b->ne[0]) failed
llama runner process no longer running: 2 error: ...
```

Upstream confirmed in [ollama/ollama#14401][upstream-14401] and
[ollama/ollama#14171][upstream-14171] that the assertion fires when the
vision-token count for a page exceeds Ollama's context window. Ollama's
built-in default is **4096 tokens**, which is too small for a realistic
single-page render. Upstream user `hapm` confirmed: "Context size was
configured to 7000, works well with 8192."

[upstream-14401]: https://github.com/ollama/ollama/issues/14401
[upstream-14171]: https://github.com/ollama/ollama/issues/14171

### The fix

- The worker now wires `options.num_ctx` into every Ollama vision and text
  payload. The default for vision is **16384** (safe ceiling for commodity
  hosts, headroom for multi-page rendering at high DPI). The default for
  text-chat is **8192** (covers the 16k-char metadata-extraction prompt
  with comfortable headroom).
- Remote providers (OpenAI / Anthropic / OpenAI-compatible) ignore the
  field — the override only travels to the local Ollama runner.
- Operators can re-tune both numbers from the Settings → AI section.
  Memory-constrained Ollama hosts can lower them; very-high-DPI multi-page
  scanners can raise them.
- All seven locales (en/de/fr/es/it/nl/pl) ship the new labels and hints.

### Defense in depth (carried over from v1.5.0)

The v1.5.0 fallback machinery stays in place:

- **Crash detection** — `is_vision_model_runtime_crash` still recognises
  the GGML_ASSERT / "runner process no longer running" signatures and
  retries the page on a fallback model (operator's `fallback_vision_model`
  setting or a hardcoded safe-default chain).
- **Startup requeue** — `run_startup_vision_crash_requeue` still lifts
  pre-fix `failed` OCR jobs back into the queue on worker boot. Because
  the requeue clears the matching `error_message`, it is naturally
  idempotent — subsequent restarts do not double-fire.

The **expected behavior after v1.5.1** is that this fallback machinery
becomes dormant: `vision_model_fallback_used=true` counts trend toward
zero, and primary glm-ocr completes without crash.

### What to watch in production after deploy

- Worker startup log line:
  `setting vision options.num_ctx and text options.num_ctx for Ollama calls`
  with the configured values. If you see 16384 / 8192, the fix is live.
- `vision_model_fallback_used=true` log count should **drop toward zero**.
  The fallback existed to mask the crash; the crash should no longer fire.
- Previously-dead OCR jobs killed by the GGML_ASSERT signature are
  automatically requeued on worker startup (one-shot) and should now
  succeed on the first attempt without a fallback hop.
- Operator does nothing. Full-Auto stays Full-Auto.

### Compatibility

- New settings (`ai.ollama_vision_num_ctx`, `ai.ollama_text_num_ctx`) carry
  `#[serde(default = ...)]` so existing `RuntimeSettings` rows deserialize
  without migration.
- `ChatRequest.num_ctx` and `VisionRequest.num_ctx` are new optional fields
  with `#[serde(default)]`. Existing API consumers that build these structs
  manually need to add `num_ctx: None` (compiler-enforced in the workspace).

## v1.4.1 — Migration compatibility fix

- Fixes migration `0019_metadata_stage.sql` by making
  `jobs.stage_priority` a stored generated column. PostgreSQL 18 does not
  support indexes on virtual generated columns.
- Extends the PostgreSQL 18 migration smoke test to assert that
  `jobs.stage_priority` is stored before a release can pass validation.

## v1.4.0 — Consolidated metadata stage + age-derived job scheduling

Two coupled architectural changes — the biggest single feature shipped in
v1.x. The pipeline default sequence becomes `Ocr -> Metadata` (replacing six
per-field stages), and the worker drains newer documents first with manual
triggers jumping the queue.

### Headline changes

**Consolidated metadata stage**

- New `Stage::Metadata` runs ONE LLM call that yields up to six fields —
  title, document_type, correspondent, document_date, tags, custom fields.
- Net effect on an end-to-end run: ~6x fewer LLM round-trips, ~5x less total
  token spend (one system+context prompt rather than six), drastically lower
  wall-clock latency per document.
- The six legacy per-field stages (`Title`, `DocumentType`, `Correspondent`,
  `DocumentDate`, `Tags`, `Fields`) remain in the `Stage` enum and stay
  selectable for prompt-management UX; in-flight runs queued before v1.4.0
  continue to drain through those code paths unchanged.
- Operators can still opt out of individual fields via
  `WorkflowSettings::enabled_stages` — the consolidated prompt builder reads
  the list and omits disabled fields from both the requested-key set and
  the closed-vocabulary allowlists.

**Age-derived priority scheduling**

- `jobs.payload` now carries TWO priority values:
  - `priority` — cross-run ordering (smaller wins). Manual triggers stamp
    `0`; the auto-selector / paperless ingest delta-sync / `queue_missing_*`
    bulk path stamps `1_000_000 - paperless_document_id` so a fresh scan
    drains its full pipeline ahead of older queued documents.
  - `stage_priority` — within-run stage ordering (smaller wins). Preserves
    the OCR -> Metadata -> ... order inside a single run regardless of the
    cross-run priority value.
- `claim_jobs` orders by `priority, stage_priority, run_after, created_at`
  and uses `stage_priority` in the within-run dependency subquery, so the
  two roles are cleanly split.
- The "Trigger OCR" / "Trigger Tags" / Reviews "Re-queue" UI buttons emit
  priority 0 so an operator-initiated action always jumps ahead of the
  backlog.

### Compatibility & backward-compat policy

- `Stage::all_business_stages()` now returns `[Ocr, Metadata]`. Existing
  rows in `pipeline_runs.stages` are NOT migrated; the worker keeps
  matching the legacy variants and produces review items as before.
- Migration `0019_metadata_stage.sql`:
  - adds `document_inventory.metadata_status` (default `'unknown'`),
  - adds `jobs.stage_priority` as a stored generated column derived from
    `payload->>'stage_priority'` with a fallback to the legacy
    `payload->>'priority'` so pre-existing rows preserve their original
    stage ordering. It is stored because PostgreSQL 18 does not support
    indexes on virtual generated columns.
- Frontend `Stage` union, `defaultStageStatus`, `promptStageOrder`, and
  Reviews per-field renderer all gain a `metadata` entry. All seven
  completeLocales (en/de/fr/es/it/nl/pl) ship `stage.metadata`.

### What to watch in production after deploy

- Dashboard StageMatrix should grow a new "Metadata" row that accumulates
  throughput as new runs drain. Legacy rows (Title, Tags, ...) should
  trend toward zero as in-flight runs finish.
- A bulk re-scan or manual trigger should observe a drop in the per-doc
  wall-clock by roughly 5-6x compared to v1.3.x.
- Verify priority scheduling: trigger a manual run on a low doc id while a
  large auto-selector backlog is queued. The dashboard live timeline should
  show the manual document drain ahead of the auto-selected ones.

### Upgrade notes

- PostgreSQL 18 or newer (unchanged).
- Stop workers, run the API to apply migration `0019_metadata_stage.sql`,
  start workers.
- No backfill required for `document_inventory.metadata_status` — the
  selector consults both the consolidated column and the legacy per-field
  columns until v1.5.

## Milestone #14 — Post-v1.1 hardening (closed)

All 25 hardening issues are landed. Highlights:

- Backend perf and safety: audit-event indexes (#80), deduped dashboard
  helper queries (#81), `queue_missing` SQL LIMIT push-down, snapshot
  off the read path (#97), bounded `provider_usage` joins (#99), typed
  SQL allowlists for status counts and stage-keyed queries (#91).
- Security: constant-time CSRF token comparison and threat-model docs
  (#83), explicit request body size limits with per-route overrides
  (#87), login IP rate limiter, SSRF URL validator, recovery permission
  alignment surfacing `permissions.read_runs` / `permissions.write_runs`
  on `/auth/me` (#98), prompt-injection threat model and cookie-secure
  default documentation (#100).
- Worker: retry backoff jitter (#88), O(1) tag lookup (#92), typed
  error variants (`PaperlessError`, `AiProviderError`) replacing the
  bulk of substring-based failure classification (#100).
- Frontend: shared ErrorBoundary at shell/tab/dashboard layers (#82),
  App.tsx extraction (Settings/Prompts/Audit/Users/DocumentChat code
  splits), inventory and reviews row memoisation, dashboard sparkline
  HashMap lookups (#100), real a11y fix in the dashboard stage matrix
  (caught by the new render test).
- Testing & tooling: pure dashboard helpers extracted and unit-tested
  in archivist-db; vitest + jest-axe coverage for `computeHealthScore`,
  `parseDocumentIds`, the review patch helpers, and shell-level axe
  assertions for `<Dashboard>`, `<Reviews>` and `<SettingsPage>` (#101).
  Informational `pnpm i18n:check` script reporting untranslated DE
  values (#100).
- Docs: ADR-010 on snapshot-bucket trade-offs, SECURITY_DESIGN.md
  section 4.2 (cookie Secure flag) and 14.1 (prompt-injection threat
  model).

## v1.1.2

- Workflow card stack layout fix on the operations strip.
- HealthBadge wrap fix and per-provider sparkline data wired up from
  bucketed series.
- Chart-pattern fills and a proper tablist under 1100 px viewport.
- Frontend a11y smoke test (axe-core) wired into the static check.

## v1.1.1

- Apply `rustfmt` to dashboard enrichment code so the workspace
  formatting check stays green.

## v1.1.0 — Operations Dashboard Overhaul (Milestone #13)

Operations-first refresh of the dashboard.

- AlertsBar with severity grouping and quick links to recovery actions.
- HealthBadge consolidating Paperless, providers, and worker liveness.
- StageMatrix with per-stage status, throughput, and failure rates.
- CostPanel with provider, model, and time-range breakdowns; cost is
  surfaced as `Option` (no fabricated zeros).
- MaintenanceDrawer for safe, low-traffic operator actions.
- A11y pass on dashboard pills, tabs, and chart fallback contexts.
- Renamed `frontend/package.json` to `1.1.0` (now `1.1.2` after the
  v1.1.1 and v1.1.2 follow-ups above).

## v1.0.0 GA

Paperless Archivist v1.0.0 is the first GA-ready release of the secure AI
automation layer for Paperless-ngx.

### Major Capabilities

- Rust API and worker with PostgreSQL 18 storage.
- React + TypeScript frontend.
- Paperless REST API integration only; no direct Paperless database writes.
- OCR, title, correspondent, document type, document date, tag, and custom-field
  extraction stages.
- Review mode, auto-select with review, and full autopilot.
- Completion tags and trigger-tag cleanup.
- Document inventory, backlog dashboard, live processing status, and recovery
  tools.
- Document Chat/RAG with citations to Paperless documents.
- UI-managed runtime settings, model providers, local Ollama model discovery,
  prompt workbench, users, sessions, and scoped API tokens.
- Local login, Argon2id, sessions, CSRF, RBAC, OIDC SSO, audit log, secret
  redaction, encrypted secret references, and audit integrity checks.
- Hardened Docker Compose profiles and generic Kubernetes package.

### Upgrade Notes

- PostgreSQL 18 or newer is required.
- Stop workers before upgrading.
- Back up PostgreSQL and `ARCHIVIST_SECRET_KEY`.
- Start the API first and wait for migrations/readiness.
- Start workers after the API is healthy.
- Run Paperless consistency check after upgrade.

### Rollback Notes

Rollback to an older version after migrations requires restoring a database
backup from before the upgrade. Do not run older binaries against a newer schema
unless that release explicitly documents compatibility.

### Known Limitations

- Non-English UI languages beyond English and German use the English text
  fallback until translated catalogs are added.
- Public Kubernetes manifests are generic and must be patched for the target
  cluster, secrets, image registry, ingress, and storage policy.
- Benchmark results are synthetic and should be repeated on the operator's
  PostgreSQL storage for very large archives.
