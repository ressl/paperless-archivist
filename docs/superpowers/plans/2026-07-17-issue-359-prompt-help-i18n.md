# Issue #359: Prompt Help i18n Implementation Plan

**Goal:** Resolve every OCR and metadata stage-help string through typed i18n keys so changing the active locale updates labels, guidance, safety rules, and examples without reloading the page.

### Task 1: Define the typed translation boundary with a failing render test

**Files:**
- Modify: `frontend/src/data/promptHelp.ts`
- Create: `frontend/src/prompts/Prompts.i18n.test.tsx`

- [x] Represent labels, purpose, expected output, safety items, and examples as `MessageKey` values rather than user-facing English strings.
- [x] Add a mounted locale-switch test that first proves the current hard-coded guidance remains English after switching to German.
- [x] Keep stage ordering and list/example shapes statically typed.

### Task 2: Localize and render all stage help

**Files:**
- Modify: `frontend/src/prompts/Prompts.tsx`
- Modify: `frontend/src/i18n/messages.ts`
- Modify: `frontend/src/i18n/locales/{de,es,fr,it,nl,pl}.ts`

- [x] Resolve stage-help definitions with the current `t()` function on every render.
- [x] Add complete OCR and metadata guidance in English, German, Spanish, French, Italian, Dutch, and Polish.
- [x] Render localized examples alongside expected output and safety guidance.
- [x] Prove a live locale change updates already-mounted guidance and examples without a reload.

### Task 3: Verify and deliver

- [x] Run focused and full frontend tests, i18n parity, typecheck, build, contract/lint checks, and `git diff --check`.
- [ ] Obtain independent review, commit, push, and close #359 only after both branch and MR pipelines are green.
