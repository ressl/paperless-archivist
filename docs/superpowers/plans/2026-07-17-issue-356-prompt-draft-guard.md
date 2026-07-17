# Issue #356: Prompt Draft Guard Implementation Plan

**Goal:** Prevent stage and version navigation from silently replacing an unsaved prompt draft while preserving direct navigation for a clean editor and the existing activation reload guard.

### Task 1: Specify navigation behavior with failing tests

**Files:**
- Create: `frontend/src/prompts/Prompts.draft-guard.test.tsx`

- [ ] Prove clean stage navigation remains immediate.
- [ ] Prove dirty stage navigation supports cancel and explicit discard.
- [ ] Prove dirty version navigation supports cancel and explicit discard.
- [ ] Prove the dialog exposes accessible semantics, Escape cancellation, and focus restoration.
- [ ] Keep the existing activate-with-draft regression green.

### Task 2: Add one guarded selection path

**Files:**
- Modify: `frontend/src/prompts/Prompts.tsx`
- Modify: `frontend/src/styles/app.css`

- [ ] Route stage buttons, version select, and version-history buttons through one pending-selection guard.
- [ ] Preserve draft and controlled selection on cancel; apply the pending target once on discard.
- [ ] Add a modal alert dialog with a labelled description, safe initial focus, trapped Tab navigation, Escape cancellation, and focus restoration.

### Task 3: Localize and verify

**Files:**
- Modify: `frontend/src/i18n/messages.ts`
- Modify: `frontend/src/i18n/locales/{de,es,fr,it,nl,pl}.ts`

- [ ] Add complete locale strings for title, explanation, cancel, and discard actions.
- [ ] Run focused tests, full frontend tests, i18n parity, accessibility static checks, typecheck, build, and diff checks.
- [ ] Obtain independent review, commit, push, and close #356 only after the MR pipeline is green.
