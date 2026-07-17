# Issue #363: Safe Custom Provider Removal Implementation Plan

**Goal:** Let operators remove obsolete custom providers without dangling references or index-state corruption while keeping built-in presets disable-only.

### Task 1: Define removal identity and state helpers

**Files:**
- Modify: `frontend/src/settings/SettingsPage.tsx`

- [ ] Add red UI tests for removable custom providers, protected built-ins, referenced-provider blocking, and successor draft-state stability.
- [ ] Identify built-in presets case-insensitively from the canonical preset names.
- [ ] Remove an indexed provider while shifting draft-secret/model state and invalidating stale in-flight model loads.

### Task 2: Implement safe removal UX

**Files:**
- Modify: `frontend/src/settings/SettingsPage.tsx`
- Modify: `frontend/src/settings/sections/ProviderCard.tsx`
- Modify: `frontend/src/i18n/messages.ts`
- Modify: `frontend/src/i18n/locales/{de,es,fr,it,nl,pl}.ts`

- [ ] Block removal when the provider is the default or is referenced by a stage, naming every blocking reference.
- [ ] Confirm unreferenced custom removal with the local draft/model-state and Save impact.
- [ ] Expose no Remove action for built-ins and explain that they can only be disabled.

### Task 3: Prove persistence and delivery

**Files:**
- Modify: `frontend/src/settings/SettingsPage.provider-test.test.tsx`

- [ ] Prove a removed custom provider stays absent after Save response and reload.
- [ ] Prove successor API-key/model state never receives the removed provider's indexed state.
- [ ] Run focused and full frontend verification, independent review, commit/push, both pipelines, and close #363 only when green.
