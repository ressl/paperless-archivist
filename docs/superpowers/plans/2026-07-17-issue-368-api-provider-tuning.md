# Issue #368: API Provider Tuning Plan

**Goal:** Make every API-side text consumer use the effective tuning and request timeout of the provider it actually selected.

### Task 1: Pin provider-specific resolution

**Files:**
- Modify: `crates/archivist-core/src/lib.rs`
- Modify: `crates/archivist-api/src/main.rs`

- [x] Expose the existing effective-tuning resolver for a concrete provider.
- [x] Cover two providers with different reasoning, output, structured-output, context, and timeout profiles.
- [x] Store the resolved profile with `ApiProvider` so model overrides cannot switch tuning sources.

### Task 2: Apply tuning to every API text consumer

**Files:**
- Modify: `crates/archivist-api/src/main.rs`

- [x] Apply the selected provider profile to Prompt Tester requests after provider/model selection.
- [x] Preserve the existing draft-provider test contract through the common provider representation.
- [x] Apply the default text provider profile to Document Chat requests.
- [x] Construct Ollama, OpenAI-compatible, and Anthropic API clients with the resolved request timeout.

### Task 3: Prove consumer and wire behavior

**Files:**
- Modify: `crates/archivist-api/src/main.rs`

- [x] Add pure request tests that pin all request-relevant tuning values without cross-provider leakage.
- [x] Extend mock-wire coverage for the selected provider payload and configured timeout.
- [x] Verify that no OpenAPI/client regeneration is needed because request/response schemas stay unchanged.

### Task 4: Remove obsolete limitations and deliver

**Files:**
- Modify: `docs/FEATURE_REFERENCE.md`
- Modify: `docs/USER_GUIDE.md`

- [x] Replace the obsolete API-helper tuning limitation with the now-shared behavior.
- [x] Run focused red/green tests, API/Core/AI tests, workspace checks, formatting, lint, and documentation checks.
- [x] Obtain an independent Critical/Important review and resolve every finding.
- [x] Commit/push, verify both GitLab pipelines, document evidence, and close #368 only when green.
