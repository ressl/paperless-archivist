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

### Opt-in SGLang/MiniMax M3 live contract

[`scripts/verify/sglang_minimax_m3_contract.mjs`](../scripts/verify/sglang_minimax_m3_contract.mjs)
is the narrow exception to the otherwise manual LLM-runtime layer. It checks
the exact public model `ressl/MiniMax-M3-uncensored-NVFP4` against an
operator-supplied OpenAI-compatible `/v1` endpoint. It does not run in normal
merge requests and does not process Paperless documents or personal data.

The contracts are independently selectable:

| Contract | Live assertion |
| --- | --- |
| `models` | `GET /v1/models` contains the exact configured ID. |
| `text` | A deterministic text completion returns a sentinel final answer. |
| `schema` | An adversarial prompt asks for a forbidden object; strict closed JSON Schema still returns its only allowed object. |
| `reasoning-disabled` | `thinking_mode=disabled` returns final content and no separated reasoning. |
| `reasoning-enabled` | `thinking_mode=enabled` returns final content plus separated reasoning. |
| `reasoning-adaptive` | `thinking_mode=adaptive` returns final content plus separated reasoning. |
| `tool` | A specifically forced OpenAI tool call returns the closed sentinel arguments. |
| `image` | The answer identifies the dominant colour of an embedded, metadata-free synthetic PNG and therefore cannot be copied from the prompt. |

Configuration is environment-only; there are no endpoint, model or secret
command-line flags:

| Variable | Required/default |
| --- | --- |
| `SGLANG_CONTRACT_BASE_URL` | Required credential-free URL ending in `/v1`. Never written to the report. |
| `SGLANG_CONTRACT_API_KEY_FILE` | Optional path to a mounted secret file. The key is read in memory and redacted from every diagnostic. |
| `SGLANG_CONTRACTS` | Optional comma-separated subset; defaults to all contracts. |
| `SGLANG_CONTRACT_MODEL` | Optional assertion input; if set, it must equal the exact public M3 model above or configuration fails. |
| `SGLANG_CONTRACT_MODEL_REVISION` | Defaults to the model revision accepted in ADR-014. |
| `SGLANG_CONTRACT_RUNTIME_REVISION` | Defaults to the pinned SGLang revision accepted in ADR-014. |
| `SGLANG_CONTRACT_IMAGE_DIGEST` | Defaults to the pinned public SGLang image digest accepted in ADR-014. |
| `SGLANG_CONTRACT_TIMEOUT_MS` | Per-request timeout, default 180000. |
| `SGLANG_CONTRACT_MAX_RESPONSE_BYTES` | Response ceiling, default 2 MiB and hard-capped at 16 MiB. |
| `SGLANG_CONTRACT_MAX_TOKENS` | Per-completion output cap, default 1024. |
| `SGLANG_CONTRACT_VISION_SCOPE` | `informational` by default; set `gate` only after the ADR vision gate is approved. |
| `SGLANG_CONTRACT_REPORT_FILE` | Optional JSON report path, created mode 0600. |

Run a single public-safe probe, for example:

```bash
SGLANG_CONTRACT_BASE_URL='https://sglang.example.invalid/v1' \
SGLANG_CONTRACTS='models' \
node scripts/verify/sglang_minimax_m3_contract.mjs
```

Run the entire text-first matrix by omitting `SGLANG_CONTRACTS`. Exit code `0`
means every release-gating contract passed; an image-only failure is recorded
as `passed_with_informational_failure`. Exit code `1` means a live contract
failed, and `2` means configuration was invalid. Setting
`SGLANG_CONTRACT_VISION_SCOPE=gate` makes image failure return `1`.

For an operator acceptance run that excludes the informational image probe,
select the release-gating text matrix explicitly:

```bash
SGLANG_CONTRACT_BASE_URL=https://sglang.example.invalid/v1 \
SGLANG_CONTRACT_API_KEY_FILE=/run/secrets/sglang-key \
SGLANG_CONTRACTS=models,text,schema,reasoning-disabled,reasoning-enabled,reasoning-adaptive,tool \
node scripts/verify/sglang_minimax_m3_contract.mjs
```

Runtime/parser prerequisites, the API/worker validation order, and failure
remediation are in the
[operations runbook](OPERATIONS.md#sglangminimax-m3-operations).

The versioned report contains public model/runtime/image fingerprints,
per-contract latency, status, and at most 512 characters of redacted failure
diagnostics. It never contains the endpoint, authorization value, request
prompt, provider response, reasoning trace, or tool arguments. The endpoint is
represented only by SHA-256 so reports from the same deployment can be
correlated without publishing its address.

Ordinary CI runs the complete local mock and negative matrix through
`public:sglang:minimax-m3:contract-mock`; the authoritative internal blueprint
also reaches the same offline matrix through the frontend `pnpm test` script.
The live job is exposed in the public-safe project pipeline as
`sglang:minimax-m3:live-contract` only on a protected ref when the protected
endpoint variable is present; it remains a manual action and stores only the
redacted JSON report. Use a protected GitLab file variable for the optional API
key path.

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
