# Issue #361: Dashboard WAI-ARIA Tabs Implementation Plan

**Goal:** Implement the compact dashboard switcher as a complete, automatically activated WAI-ARIA tabs pattern with paired panels and behavior-level accessibility tests.

### Task 1: Capture the missing interaction contract

**Files:**
- Modify: `frontend/src/dashboard/Dashboard.render.test.tsx`

- [x] Render the compact layout by controlling `matchMedia` in the integration test.
- [x] Assert the tab/tab-panel ID relationships, roving `tabIndex`, and `hidden` panel state.
- [x] Prove ArrowLeft/ArrowRight wrapping and Home/End navigation move focus and activate the matching panel.
- [x] Run `jest-axe` against the mounted compact tab interface.

### Task 2: Implement automatic-activation tabs

**Files:**
- Modify: `frontend/src/dashboard/Dashboard.tsx`
- Modify: `frontend/src/dashboard/TrendCharts.tsx`

- [x] Give every tab and panel a stable, reciprocal ID/ARIA relationship.
- [x] Keep exactly one tab in the keyboard sequence with roving `tabIndex`.
- [x] Implement automatic activation for ArrowLeft/ArrowRight/Home/End and document that focus and activation move together.
- [x] Hide inactive compact panels with the semantic `hidden` attribute while preserving the wide layout.

### Task 3: Verify and deliver

- [x] Run focused tests red then green, full frontend tests, lint/contracts, typecheck, build, and `git diff --check`.
- [ ] Obtain independent review, commit, push, inspect both remote test traces, and close #361 only after branch and MR pipelines are green.
