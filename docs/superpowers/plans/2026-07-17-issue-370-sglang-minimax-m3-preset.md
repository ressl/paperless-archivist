# Issue #370: SGLang MiniMax M3 Preset Plan

**Goal:** Add a safe, disabled SGLang/MiniMax M3 provider preset and exact text-model catalog entry without creating a new protocol kind, shipping an endpoint, enabling vision, or overwriting operator configuration.

### Task 1: Pin the core preset and upgrade contract

**Files:**
- Modify: `crates/archivist-core/src/lib.rs`
- Modify: `crates/archivist-ai/src/lib.rs`

- [x] Add failing tests for the exact provider name/model, `openai_compatible` kind, disabled state, empty Base URL, absent secret, absent vision model, and blank tuning.
- [x] Add idempotent upgrade tests proving case/whitespace-insensitive provider detection and preservation of existing operator fields.
- [x] Centralize the exact MiniMax M3 identity and reuse it in AI wire behavior, preset construction, and catalog construction.
- [x] Append the preset only when absent; never rewrite an existing same-name provider.

### Task 2: Add the non-default catalog entry

**Files:**
- Modify: `crates/archivist-core/src/lib.rs`
- Modify: `frontend/src/modelCatalog.ts`

- [x] Add the exact text-only model ID to the Core and frontend fallback catalogs.
- [x] Keep the generic Qwen text entry as the sole OpenAI-compatible recommendation.
- [x] Add idempotent catalog-upgrade coverage that preserves an operator-customized M3 entry.
- [x] Do not add an M3 vision entry.

### Task 3: Render the safe preset and verify discovery

**Files:**
- Modify: `frontend/src/modelCatalog.ts`
- Modify: `frontend/src/settings/sections/ProviderCard.tsx`
- Modify: `frontend/src/settings/SettingsPage.provider-test.test.tsx`
- Modify: `frontend/src/settings/SettingsPage.a11y.test.tsx`
- Modify: `crates/archivist-api/src/main.rs`

- [x] Add the disabled preset as an idempotent frontend fallback for older settings payloads.
- [x] Keep built-in provider identity/kind immutable and M3 vision unset in the Settings UI.
- [x] Prove empty-URL activation is blocked by existing frontend and backend validation.
- [x] Prove the exact served ID is retained from an OpenAI-compatible `/v1/models` mock.
- [x] Cover preset rendering, picker value, activation validation, and accessibility.

### Task 4: Verify unchanged protocol and deliver

**Files:**
- Verify: `openapi/openapi.yaml`
- Verify: `frontend/src/api/schema.ts`

- [x] Prove no `sglang` provider kind is introduced in Core, frontend, or OpenAPI.
- [x] Run focused red/green tests, workspace tests, Clippy, formatting, frontend tests/typecheck/lint/build, OpenAPI contracts, and documentation checks.
- [x] Obtain an independent Critical/Important review and resolve every finding.
- [x] Commit/push, verify both GitLab pipelines, document evidence, and close #370 only when green.
