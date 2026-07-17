# Issue #357: Inventory Selection Context Implementation Plan

**Goal:** Prevent bulk inventory actions from retaining or submitting document IDs that no longer belong to the visible query result, while preserving selection across an explicit load-more operation.

### Task 1: Specify the selection contract with failing tests

**Files:**
- Create: `frontend/src/inventory/Inventory.selection.test.tsx`

- [ ] Prove filter and committed-search changes clear selected rows, count, and header state.
- [ ] Prove a manual refresh clears selection even when the refreshed result contains the same IDs.
- [ ] Prove bulk rerun never submits a previously selected hidden ID.
- [ ] Prove load more preserves existing visible selections and does not auto-select appended rows.

### Task 2: Bind selection to the visible query context

**Files:**
- Modify: `frontend/src/inventory/Inventory.tsx`

- [ ] Clear selection immediately when the filter/query context changes or a first-page refresh starts.
- [ ] Clear selection again when the latest first-page response replaces the result, preventing in-flight refresh races from reviving stale choices.
- [ ] Derive checkbox state, selected count, clear action, and bulk payload from the intersection of selected and visible IDs.
- [ ] Keep load-more append behavior selection-preserving and explicitly covered by tests.

### Task 3: Verify and deliver

- [ ] Run focused and full frontend tests, lint, i18n/accessibility checks, typecheck, and build.
- [ ] Obtain independent review, commit, push, and close #357 only after the MR pipeline is green.
