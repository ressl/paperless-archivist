# Issue #358: Provider Draft Test Implementation Plan

**Goal:** Make the provider connection test execute exactly the visible unsaved provider draft, including request-relevant tuning and a transient secret, without persisting or reflecting sensitive input.

### Task 1: Define the API contract and failing backend tests

**Files:**
- Modify: `openapi/openapi.yaml`
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Define closed request/response schemas with provider identity, kind, URL, text model, tuning, optional saved-secret reference, and write-only transient secret.
- [ ] Prove a draft mock endpoint is called while the saved endpoint is not.
- [ ] Prove model and request-relevant tuning reach the provider wire request.
- [ ] Prove the transient secret is used only as authorization input and is absent from success/error responses.

### Task 2: Execute the transient draft safely

**Files:**
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Require settings-write user-session authority for arbitrary draft probes.
- [ ] Validate the draft URL with the outbound-request guard and accept saved secret IDs only when they belong to a persisted AI provider.
- [ ] Resolve effective tuning against saved global fallbacks, but let the draft provider override request settings and timeout.
- [ ] Return the tested provider and model on both success and failure without persisting the request.

### Task 3: Send the visible frontend draft

**Files:**
- Modify: `frontend/src/api/client.ts`
- Modify: `frontend/src/settings/SettingsPage.tsx`
- Create: `frontend/src/settings/SettingsPage.provider-test.test.tsx`
- Modify: `frontend/src/i18n/messages.ts`
- Modify: `frontend/src/i18n/locales/{de,es,fr,it,nl,pl}.ts`

- [ ] Send the selected draft's name, kind, URL, visible text model, complete tuning object, saved secret reference, and currently typed secret.
- [ ] Use response provider/model identity for unambiguous success and error feedback.
- [ ] Update all locale copy so the UI accurately states that the visible unsaved draft is tested without saving it.
- [ ] Regenerate the TypeScript client and prove the exact UI payload with a component test.

### Task 4: Verify and deliver

- [ ] Run backend tests, OpenAPI contracts/regeneration, focused and full frontend tests, lint, i18n/accessibility, typecheck, build, and security checks.
- [ ] Obtain independent review, commit, push, and close #358 only after the MR pipeline is green.
