# Issue #368: API Provider Tuning Plan

**Goal:** Make every API-side text consumer use the effective tuning and request timeout of the provider it actually selected.

### Task 1: Pin provider-specific resolution

**Files:**
- Modify: `crates/archivist-core/src/lib.rs`
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Expose the existing effective-tuning resolver for a concrete provider.
- [ ] Cover two providers with different reasoning, output, structured-output, context, and timeout profiles.
- [ ] Store the resolved profile with `ApiProvider` so model overrides cannot switch tuning sources.

### Task 2: Apply tuning to every API text consumer

**Files:**
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Apply the selected provider profile to Prompt Tester requests after provider/model selection.
- [ ] Preserve the existing draft-provider test contract through the common provider representation.
- [ ] Apply the default text provider profile to Document Chat requests.
- [ ] Construct Ollama, OpenAI-compatible, and Anthropic API clients with the resolved request timeout.

### Task 3: Prove consumer and wire behavior

**Files:**
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Add pure request tests that pin all request-relevant tuning values without cross-provider leakage.
- [ ] Extend mock-wire coverage for the selected provider payload and configured timeout.
- [ ] Verify that no OpenAPI/client regeneration is needed because request/response schemas stay unchanged.

### Task 4: Remove obsolete limitations and deliver

**Files:**
- Modify: `docs/FEATURE_REFERENCE.md`
- Modify: `docs/USER_GUIDE.md`

- [ ] Replace the obsolete API-helper tuning limitation with the now-shared behavior.
- [ ] Run focused red/green tests, API/Core/AI tests, workspace checks, formatting, lint, and documentation checks.
- [ ] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify both GitLab pipelines, document evidence, and close #368 only when green.
