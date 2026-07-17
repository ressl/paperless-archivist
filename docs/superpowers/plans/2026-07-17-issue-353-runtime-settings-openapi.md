# Issue #353 RuntimeSettings OpenAPI Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make OpenAPI the complete, reproducible type contract for runtime settings and provider tuning, then consume the generated schemas in the frontend.

**Architecture:** Describe every serialized `RuntimeSettings` branch as a closed OpenAPI 3.1 response schema and every Serde-defaulted write branch as a separate input schema, composed through `$ref`. Checked-in complete-response and partial-input fixtures are consumed by both `archivist-core` and the recursive OpenAPI validator. Generated `components` aliases replace handwritten frontend duplicates while the UI-only business-stage subset remains an `Exclude` of the generated pipeline stage.

**Tech Stack:** Rust/Serde, OpenAPI 3.1, openapi-typescript, TypeScript 6, Node.js.

## Global Constraints

- Implement GitLab issue #353 only; router path parity remains #354.
- Match Serde wire names, nullable values, and fields omitted by `skip_serializing_if`.
- Keep provider tuning members optional and nullable because settings updates may inherit individual values by omitting or nulling them.
- Close object schemas unless a map is intentionally open, such as provider secrets.
- Keep client generation deterministic and commit the generated schema.

### Task 1: Establish a failing cross-language contract

**Files:**
- Add: `openapi/fixtures/runtime-settings.json`
- Add: `scripts/verify/runtime_settings_openapi_contract.mjs`
- Modify: `frontend/package.json`
- Add: `crates/archivist-core/tests/runtime_settings_contract.rs`

- [x] Add a representative complete settings fixture covering nested settings, optional/null values, provider tuning, and the model catalog.
- [x] Prove the fixture round-trips through Serde without shape drift.
- [x] Add an OpenAPI fixture validator and demonstrate that the current open `RuntimeSettings` contract fails.

### Task 2: Define the complete OpenAPI schema graph

**Files:**
- Modify: `openapi/openapi.yaml`
- Modify: `frontend/src/api/schema.ts`

- [x] Add closed schemas for every `RuntimeSettings` branch, workflow tags/rules, field mappings, AI providers, provider tuning, and model catalog entries.
- [x] Document reasoning effort, output-token cap, structured-output mode, request timeout, inheritance/null behavior, examples, and numeric bounds.
- [x] Match Serde-required response properties while leaving `skip_serializing_if` properties optional.
- [x] Regenerate `schema.ts` and prove a second generation is diff-free.

### Task 3: Consume generated frontend types and verify delivery

**Files:**
- Modify: `frontend/src/api/client.ts`

- [x] Replace handwritten settings/provider/catalog/enums with aliases of generated component schemas.
- [x] Preserve the UI business-stage subset as `Exclude<PipelineStage, 'apply'>`.
- [x] Run the Serde fixture test, OpenAPI fixture validator, generator diff, TypeScript typecheck, frontend tests, and production build.
- [x] Run repository formatting/diff checks, obtain independent review, commit, push, and close #353 only after the MR pipeline is green.
