# Issue #369: Metadata Prompt Tester Plan

**Goal:** Replace the consolidated-metadata placeholder with a side-effect-free prompt test that uses the worker's live allowlists, enabled fields, limits, language context, prompt builder, schema builder, and safe parser.

### Task 1: Pin the worker-equivalent request contract

**Files:**
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Add failing tests for enabled metadata fields, filtered dynamic allowlists, custom-field type hints, runtime limits, and the generated response schema.
- [ ] Prove that an editor system prompt replaces only the system component while the generated user prompt and schema stay intact.
- [ ] Load only the allowlists and custom fields needed by the active workflow flags.
- [ ] Detect language without persisting worker state and build the request with `prompt_for_metadata` plus `schema_for_metadata`.

### Task 2: Return a typed, safely validated result

**Files:**
- Modify: `crates/archivist-api/src/main.rs`
- Modify: `openapi/openapi.yaml`
- Regenerate: `frontend/src/api/schema.ts`
- Modify: `frontend/src/api/client.ts`

- [ ] Add failing tests for valid, partial, malformed, non-object, wrong-type, and unknown-field responses.
- [ ] Return the retained typed suggestion together with value-free parser diagnostics.
- [ ] Translate contract violations into validation errors and safe omission/date diagnostics into warnings.
- [ ] Define the OCR/metadata parsed-result union in OpenAPI and regenerate the frontend schema.

### Task 3: Prove the UI success and error paths

**Files:**
- Add: `frontend/src/prompts/Prompts.test-runner.test.tsx`

- [ ] Test a successful metadata response including provider/model, typed parsed data, and raw text.
- [ ] Test validation errors and warnings from a partial/invalid response.
- [ ] Preserve the existing editor request semantics and render all returned diagnostics.

### Task 4: Verify side effects, audit safety, and delivery

**Files:**
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Keep prompt testing read-only except for the required redacted audit event; do not enqueue jobs, create reviews, persist language, or patch Paperless.
- [ ] Keep audit metadata limited to stage, provider, model, sample size, duration, and validity; never store prompts or model output.
- [ ] Run focused red/green tests, API/AI/workspace checks, formatting, lint, generated-client validation, and documentation checks.
- [ ] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify both GitLab pipelines, document evidence, and close #369 only when green.
