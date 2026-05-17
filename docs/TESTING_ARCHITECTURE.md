# Testing Architecture

This doc anchors how new pipeline features are verified in Paperless
Archivist. It exists because we shipped two production bugs in the
v1.5.x window (the dedup query referencing non-existent columns;
silent dashboard action no-ops) that **compiled fine and passed every
test we had**. The architecture below is the response: each layer
catches a specific class of bug; the gaps below each layer are
explicit so we know what we are choosing not to catch.

## Layers

### 1. Unit (per-crate Rust + tsc)

- **Entry points**: `cargo test --workspace --lib --bins`, `npm run typecheck`.
- **What it catches**: type errors, pure-function logic bugs,
  serde-roundtrip regressions, frontend TS shape mismatches.
- **What it does NOT catch**: anything that touches a real database,
  HTTP endpoint behaviour, LLM output quality, or UI rendering.

Backend tests live in `#[cfg(test)] mod tests` next to the code.
Frontend tests use `vitest` for the few cases that warrant runtime
assertions (today: accessibility checks).

### 2. Integration: `migration_smoke`

- **Entry point**: `bash scripts/verify/migration_smoke.sh` (boots a
  fresh `postgres:18` container, applies every migration, runs
  `cargo test -p archivist-db --test migration_smoke`).
- **What it catches**: SQL queries that reference columns/tables that
  don't exist in the real schema. The v1.5.14 dedup bug
  (`column aa.paperless_document_id does not exist`) would have been
  caught here if every new SQL helper had a smoke-test call. **Rule:
  every new `sqlx::query(...)` in `archivist-db` gets one line in
  `migration_smoke.rs` that invokes it with throwaway ids and asserts
  `Ok(None)` / `Ok(vec![])`.**
- **What it does NOT catch**: query-result correctness, multi-row
  behaviour, transactional semantics, or anything that depends on
  realistic data.

`sqlx::query!` (compile-time-checked) would be stricter but requires
`DATABASE_URL` available at `cargo check` time, which doesn't fit the
project's offline build path. Smoke covers the same class of bug at
test time.

### 3. Contract: OpenAPI ↔ generated client

- **Entry points**: `npm run generate:client` (regenerates
  `frontend/src/api/schema.ts` from `openapi/openapi.yaml`), then
  `npm run typecheck`.
- **What it catches**: drift between the API shape the backend
  returns and the shape the frontend expects. If the backend changes
  a response and the OpenAPI doc isn't updated, the typecheck still
  passes (because schema.ts didn't change) — so this layer only
  catches drift that the author of the change *flagged* in the
  OpenAPI doc.
- **What it does NOT catch**: backend handlers that diverge from
  their declared schema. We rely on PR review and integration tests
  to catch that.

Contract changes are tracked by per-feature contract docs (see
`docs/METADATA_TRACE_CONTRACT.md` for the pattern): write the
contract first, get both backend and frontend agreement, then
implement.

### 4. End-to-end: manual prod verification

- **Entry point**: deploy a release tag, wait for the Argo rollout,
  exercise the UI against real Paperless + real LLM.
- **What it catches**: LLM output quality, real-world data shapes
  that the test fixtures don't cover, latency, real Paperless API
  quirks.
- **What it does NOT catch**: anything we don't manually click through
  before declaring the release good.

We do not have automated E2E tests today. The cost-of-entry would be
a Paperless test instance + a deterministic LLM mock; the cost is
proportional to a separate epic. Until then, manual E2E is the
honest answer — and every release-notes entry includes a "verify in
prod" recipe.

## Worked example — the metadata-trace diagnostic (v1.5.21)

The metadata-trace endpoint (`GET /api/inventory/{id}/metadata-trace`)
is a good walk-through because it hits every layer.

### Unit

- **Rust**: `compute_field_outcome` is a pure function in
  `archivist-api`. Table-driven tests cover every branch of the
  5-step decision tree (applied via audit, applied via approved
  review, pending review, rejected review, skipped overwrite-disabled,
  dropped no-proposal, skipped entity-not-found). Tests live in
  `crates/archivist-api/src/main.rs`'s `#[cfg(test)] mod tests`.
- **Frontend**: `tsc --noEmit` verifies that the drawer consumes the
  generated `MetadataTrace` / `MetadataFieldOutcome` types
  correctly. The drawer renders unconditionally for the 6
  canonical fields, so an outcome variant that's missing from the
  switch would be a TS exhaustiveness error.

### Integration

- Each of the four new `archivist-db` helpers
  (`latest_metadata_run_for_document`,
  `latest_metadata_artifact_for_run`,
  `metadata_review_items_for_run`,
  `latest_apply_audit_for_run`) gets a line in
  `migration_smoke.rs` that invokes it against the empty fresh DB
  and asserts `Ok(None)` / `Ok(vec![])`. This catches any
  non-existent-column bug before merge.

### Contract

- `MetadataTrace`, `MetadataTraceRun`, `MetadataFieldOutcome` live
  in `openapi/openapi.yaml`. The frontend client method
  `api.inventoryMetadataTrace(id)` is typed against the regenerated
  `schema.ts`. The contract doc `docs/METADATA_TRACE_CONTRACT.md`
  was written first; both implementation issues
  (#122 backend, #123 frontend) reference it as the single source
  of truth.

### Manual E2E

After deploying v1.5.21:

1. Wait for Argo rollout to settle (Dashboard alerts cleared).
2. In the Inventory tab, click the new "Diagnose" button on a
   document that recently went through the metadata stage.
3. Verify the drawer shows: current Paperless state, the run header
   (model / provider / status / `applied_at`), 6 per-field cards
   with outcome chips, and the raw `llm_suggestion` JSON.
4. Compare outcomes against what's actually in Paperless for that
   document. Mismatches indicate either a decision-tree bug in
   `compute_field_outcome` or stale `document_inventory` rows.
5. Open a document where the metadata stage *failed* to apply a
   field cleanly. The drawer's `reason` + `warnings` for that
   field should explain why (low confidence, unknown choice, etc.) —
   this is the operator-facing value of the feature.

### Second worked example — per-provider tuning (v1.5.22)

Same four-layer pattern, slightly different shape. The
`ProviderTuning` block is partly a settings-shape change (no SQL) and
partly a runtime-behaviour change (worker live-reload). What plugs
into which layer:

* **Unit** — `RuntimeSettings::effective_tuning()` is a pure
  function. Table-driven tests cover: tuning present → uses tuning;
  tuning absent → falls through to global; no matching active
  provider → first enabled; OCR-stage exception; serde-regression
  for a settings blob with no `tuning` field at all. The
  worker-pool resize is tested via a harness that swaps the
  underlying `RuntimeSettings` between `claim_jobs` cycles and
  asserts the pool grows / shrinks without aborting in-flight work.
* **Integration** — no new SQL, so `migration_smoke` is unchanged.
  The serde-regression is the upgrade-path check in this milestone.
* **Contract** — `AiRuntimeHints` + `AiLoadedModel` go in
  `openapi.yaml`; the frontend's typed `aiRuntimeHints(provider)`
  client is the contract-side gate.
* **Manual E2E** — the release-notes "verify in prod" recipe is the
  authoritative check that tuning the worker_concurrency live in
  the UI actually changes `paperless_archivist_jobs_running` in
  `/metrics`. Operator-level smoke.

## Known gaps

- **No automated E2E**. Tracked as a follow-up epic. Until then,
  the release-notes "verify in prod" section is the contract.
- **No LLM-output-quality tests**. The metadata stage is at the
  mercy of the configured model. The diagnostic endpoint surfaces
  what was proposed and what was dropped so operators can tune
  thresholds, but we do not automate that loop.
- **No load tests**. Performance is observed in prod via the
  dashboard's p95 stage duration KPI.

## Rule of thumb for new pipeline features

Before merge, ask:

1. **Does any new SQL go in?** → add the matching `migration_smoke`
   line.
2. **Does the wire shape change?** → write the contract doc first,
   update `openapi.yaml`, regenerate `schema.ts`.
3. **Does the outcome depend on a decision tree or state machine?**
   → pull it into a pure function and table-test every branch.
4. **Can the feature only be verified by clicking through the UI?**
   → write the "verify in prod" recipe into the release notes and
   add it to the post-deploy checklist.

If the answer to #4 is "yes" for everything, the feature is
under-tested. Pull at least one of the moving parts into a layer
above.
