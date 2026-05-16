# Release Notes

> Versioning policy: the Git tag (`vX.Y.Z`) is the source of truth.
> `frontend/package.json` tracks the UI release alongside the tag (currently
> `1.4.0`). The Rust workspace `Cargo.toml` files remain at the pre-GA
> internal version `0.3.2`; bumping them does not change the release.

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
  - adds `jobs.stage_priority` as a virtual column derived from
    `payload->>'stage_priority'` with a fallback to the legacy
    `payload->>'priority'` so pre-existing rows preserve their original
    stage ordering.
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
