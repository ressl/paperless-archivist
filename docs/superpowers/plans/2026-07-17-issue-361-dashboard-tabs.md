# Issue #361: Dashboard WAI-ARIA Tabs Implementation Plan

**Goal:** Implement the compact dashboard switcher as a complete, automatically activated WAI-ARIA tabs pattern with paired panels and behavior-level accessibility tests.

### Task 1: Capture the missing interaction contract

**Files:**
- Modify: `frontend/src/dashboard/Dashboard.render.test.tsx`

- [ ] Render the compact layout by controlling `matchMedia` in the integration test.
- [ ] Assert the tab/tab-panel ID relationships, roving `tabIndex`, and `hidden` panel state.
- [ ] Prove ArrowLeft/ArrowRight wrapping and Home/End navigation move focus and activate the matching panel.
- [ ] Run `jest-axe` against the mounted compact tab interface.

### Task 2: Implement automatic-activation tabs

**Files:**
- Modify: `frontend/src/dashboard/Dashboard.tsx`
- Modify: `frontend/src/dashboard/TrendCharts.tsx`

- [ ] Give every tab and panel a stable, reciprocal ID/ARIA relationship.
- [ ] Keep exactly one tab in the keyboard sequence with roving `tabIndex`.
- [ ] Implement automatic activation for ArrowLeft/ArrowRight/Home/End and document that focus and activation move together.
- [ ] Hide inactive compact panels with the semantic `hidden` attribute while preserving the wide layout.

### Task 3: Verify and deliver

- [ ] Run focused tests red then green, full frontend tests, lint/contracts, typecheck, build, and `git diff --check`.
- [ ] Obtain independent review, commit, push, inspect both remote test traces, and close #361 only after branch and MR pipelines are green.
