# MiniMax M3 Vision and OCR Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-driven-development (recommended) or executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `ressl/MiniMax-M3-uncensored-NVFP4` an optional, selectable Vision/OCR model while preserving the existing OpenAI-compatible provider architecture.

**Architecture:** Extend the built-in preset and catalog with an M3 vision capability, then carry resolved reasoning settings through `VisionRequest` so M3 image calls use the same explicit `thinking_mode` contract as text calls. Keep OCR routing under the existing default-provider and stage-override controls, and promote deterministic vision/OCR probes to live release gates.

**Tech Stack:** Rust, Tokio, Reqwest, Serde JSON, React, TypeScript, Vitest, Node test runner, OpenAI-compatible SGLang API.

## Global Constraints

- Match only the exact model ID `ressl/MiniMax-M3-uncensored-NVFP4` for MiniMax-specific wire fields.
- Keep provider kind `openai_compatible`; do not add a provider kind or database migration.
- Do not enable the provider or OCR workflow stage automatically.
- Preserve operator-authored provider URLs, secrets, stage overrides, and catalog metadata.
- Use test-first Red/Green cycles for every behavior change.

---

### Task 1: Backend preset and catalog capability

**Files:**
- Modify: `crates/archivist-core/src/lib.rs`

**Interfaces:**
- Consumes: `MINIMAX_M3_MODEL`, `SGLANG_MINIMAX_M3_PROVIDER_NAME`, `ModelCapability`.
- Produces: an M3 preset with `default_vision_model = Some(MINIMAX_M3_MODEL)` and idempotent text plus vision catalog normalization.

- [ ] **Step 1: Change the existing core tests to require vision support**

Update `sglang_minimax_m3_preset_is_disabled_text_only_and_openai_compatible` to require the exact M3 vision default and rename it to describe a multimodal preset. Update `minimax_m3_catalog_upgrade_is_idempotent_and_preserves_custom_entry` so it expects one text and one vision entry and verifies per-capability custom metadata survives normalization.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test -p archivist-core minimax_m3 --locked
```

Expected: failures showing `default_vision_model` is `None` and no Vision catalog entry exists.

- [ ] **Step 3: Implement the minimal preset and catalog changes**

Set the built-in provider field to:

```rust
default_vision_model: Some(MINIMAX_M3_MODEL.to_owned()),
```

Add a second seed entry using `ModelCapability::Vision`, modality `text+image`, and an OCR-focused `best_for` string. Replace the single-capability normalization with a loop over `[Text, Vision]` that appends only the missing capability entry and preserves any existing entry.

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run `cargo test -p archivist-core minimax_m3 --locked` and require all matching tests to pass.

- [ ] **Step 5: Commit the independently testable backend settings change**

```bash
git add crates/archivist-core/src/lib.rs
git commit -m "feat(core): expose MiniMax M3 vision capability"
```

### Task 2: MiniMax vision reasoning wire contract

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs`
- Modify: `crates/archivist-ai/tests/wire_contracts.rs`
- Modify: `crates/archivist-ai/tests/response_body_limits.rs`
- Modify: `crates/archivist-worker/src/main.rs`

**Interfaces:**
- Consumes: resolved `StageProvider.reasoning_effort` and existing `MINIMAX_M3_MODEL` exact identity.
- Produces: `VisionRequest.reasoning_effort: Option<ReasoningEffort>` and `build_openai_vision_payload(&VisionRequest) -> Result<Value>`.

- [ ] **Step 1: Add failing payload and HTTP wire tests**

Add unit cases proving all four effort levels map to `disabled`, `adaptive`, or `enabled`, missing M3 effort returns a `thinking_mode` configuration error, and a non-M3 request omits `chat_template_kwargs`. Extend `openai_vision_sends_max_tokens_on_wire` to assert the image data URI and M3 `thinking_mode` reach the mock server.

- [ ] **Step 2: Run the AI tests and verify RED**

Run:

```bash
cargo test -p archivist-ai minimax_m3_vision --locked
cargo test -p archivist-ai --test wire_contracts openai_vision --locked
```

Expected: compilation or assertion failures because `VisionRequest` has no reasoning field and the vision payload has no MiniMax mapping.

- [ ] **Step 3: Implement the minimal request builder**

Add the optional reasoning field to `VisionRequest`. Extract the existing inline OpenAI-compatible vision JSON construction into a public testable builder. Reuse one private mapping function for chat and vision:

```rust
fn minimax_m3_thinking_mode(effort: Option<ReasoningEffort>) -> Result<&'static str>
```

Only call it when `request.model == MINIMAX_M3_MODEL`. Keep base64 encoding inside the existing `spawn_blocking` path. Add `reasoning_effort: Some(provider.reasoning_effort)` when the worker creates each OCR page request; use `None` in unrelated fixtures.

- [ ] **Step 4: Run AI and worker tests and verify GREEN**

```bash
cargo test -p archivist-ai --locked
cargo test -p archivist-worker --locked
```

Both commands must exit zero.

- [ ] **Step 5: Commit the wire contract**

```bash
git add crates/archivist-ai/src/lib.rs crates/archivist-ai/tests/wire_contracts.rs crates/archivist-ai/tests/response_body_limits.rs crates/archivist-worker/src/main.rs
git commit -m "feat(ai): support MiniMax M3 vision requests"
```

### Task 3: Selectable frontend vision model

**Files:**
- Modify: `frontend/src/modelCatalog.ts`
- Modify: `frontend/src/modelCatalog.test.ts`
- Modify: `frontend/src/settings/sections/ProviderCard.tsx`
- Modify: `frontend/src/settings/SettingsPage.provider-test.test.tsx`

**Interfaces:**
- Consumes: backend `AiProvider.default_vision_model` and editable `ModelCatalogEntry` values.
- Produces: an enabled `ProviderModelSelect` for the M3 vision capability with the exact model in its choices.

- [ ] **Step 1: Add failing catalog and UI tests**

Require `modelOptions(m3Provider, 'vision')` to contain `MINIMAX_M3_MODEL`, require normalized old settings to carry a vision catalog entry and exact vision default, and require the rendered M3 vision combobox to be enabled with the exact value.

- [ ] **Step 2: Run focused Vitest cases and verify RED**

```bash
pnpm vitest run src/modelCatalog.test.ts src/settings/SettingsPage.provider-test.test.tsx --reporter=verbose
```

Expected: the model is absent from vision choices and the existing vision input is disabled.

- [ ] **Step 3: Implement the picker and frontend normalization**

Add M3 to `openAiCompatibleVisionModels`, set `sglangMinimaxM3Provider.default_vision_model` to `MINIMAX_M3_MODEL`, append a Vision catalog entry independently of the Text entry, and remove the M3-only disabled-input branch from `ProviderCard`. Keep the fixed disabled input only for MinerU.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run the focused Vitest command again and require both files to pass.

- [ ] **Step 5: Commit the frontend selection change**

```bash
git add frontend/src/modelCatalog.ts frontend/src/modelCatalog.test.ts frontend/src/settings/sections/ProviderCard.tsx frontend/src/settings/SettingsPage.provider-test.test.tsx
git commit -m "feat(frontend): make MiniMax M3 selectable for OCR"
```

### Task 4: Vision/OCR release contract and documentation

**Files:**
- Modify: `scripts/verify/sglang_minimax_m3_contract.mjs`
- Modify: `scripts/verify/sglang_minimax_m3_contract.test.mjs`
- Modify: `docs/ARCHITECTURE_DECISIONS.md`
- Modify: `docs/USER_GUIDE.md`
- Modify: `docs/OPERATIONS.md`
- Modify: `docs/TESTING_ARCHITECTURE.md`

**Interfaces:**
- Consumes: the pinned model/runtime/image identities and standard `image_url` protocol.
- Produces: gating deterministic synthetic vision and OCR contracts plus operator selection instructions.

- [ ] **Step 1: Add failing contract tests**

Change the default expected vision scope to `gate`, add `ocr` to `CONTRACT_NAMES`, and test that a synthetic embedded PNG carrying `ARCHIVIST OCR 18427` must produce exactly that text. Require a failed OCR result to make the suite exit one.

- [ ] **Step 2: Run the Node contract tests and verify RED**

```bash
node --test scripts/verify/sglang_minimax_m3_contract.test.mjs
```

Expected: missing `ocr` contract and old informational default assertions fail.

- [ ] **Step 3: Implement the gating probes and update docs**

Embed a small deterministic text-only PNG without metadata or personal data. Add the OCR request using `image_url`, explicit disabled thinking, and exact response matching. Set the default vision scope to `gate`. Revise ADR-014 from text-first exclusion to optional Vision/OCR support, document the pinned live evidence and limitations, and show both provider-default and OCR-stage-override selection flows.

- [ ] **Step 4: Run contract and documentation checks**

```bash
node --test scripts/verify/sglang_minimax_m3_contract.test.mjs
node scripts/verify/runtime_settings_openapi_contract.mjs
git diff --check
```

All commands must exit zero.

- [ ] **Step 5: Commit contracts and documentation**

```bash
git add scripts/verify/sglang_minimax_m3_contract.mjs scripts/verify/sglang_minimax_m3_contract.test.mjs docs/ARCHITECTURE_DECISIONS.md docs/USER_GUIDE.md docs/OPERATIONS.md docs/TESTING_ARCHITECTURE.md
git commit -m "docs: promote MiniMax M3 vision OCR contract"
```

### Task 5: Full verification and integration readiness

**Files:**
- Verify all modified files.

**Interfaces:**
- Consumes: Tasks 1-4.
- Produces: a clean branch whose local evidence matches CI gates.

- [ ] **Step 1: Format and run static checks**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
pnpm lint
pnpm typecheck
```

- [ ] **Step 2: Run complete tests and builds**

```bash
cargo test --workspace --locked
cargo build --workspace --locked
pnpm test
pnpm build
```

- [ ] **Step 3: Inspect the final repository state**

```bash
git diff --check origin/main...HEAD
git status --short --branch
git log --oneline --decorate origin/main..HEAD
```

Require no uncommitted files and review every commit in scope before push or merge.
