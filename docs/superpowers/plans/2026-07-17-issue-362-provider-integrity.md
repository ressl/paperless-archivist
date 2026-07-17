# Issue #362: Provider Identity and URL Integrity Implementation Plan

**Goal:** Reject ambiguous or unroutable provider settings before any secret/settings write, surface field-level UI errors, and make API/worker resolution fail closed for corrupt legacy URLs.

### Task 1: Define provider identity invariants in core

**Files:**
- Modify: `crates/archivist-core/src/lib.rs`

- [x] Add red tests for blank/whitespace names, case-insensitive duplicates, empty enabled URLs, disabled empty placeholders, and invalid default/stage references.
- [x] Normalize provider names, URLs, and references by trimming; canonicalize valid case-insensitive references to the single configured name.
- [x] Require every default/stage reference to resolve to exactly one enabled provider and reject duplicate stage overrides.

### Task 2: Gate API writes and runtime resolution

**Files:**
- Modify: `crates/archivist-api/src/main.rs`
- Modify: `crates/archivist-worker/src/main.rs`

- [x] Invoke core normalization/validation before Paperless, provider, or notification secret writes.
- [x] Canonicalize and validate provider-secret map keys before writes so unknown/ambiguous keys cannot create orphaned secrets.
- [x] Validate every enabled provider effective URL through the existing outbound URL guard, including empty URLs.
- [x] Replace provider-kind localhost fallbacks with explicit configuration errors in API and worker; add corrupt-legacy regression tests.

### Task 3: Block invalid settings in the UI

**Files:**
- Modify: `frontend/src/settings/SettingsPage.tsx`
- Modify: `frontend/src/settings/sections/ProviderCard.tsx`
- Modify: `frontend/src/lib/ui.tsx`
- Modify: `frontend/src/i18n/messages.ts`
- Modify: `frontend/src/i18n/locales/{de,es,fr,it,nl,pl}.ts`
- Modify: `frontend/src/settings/SettingsPage.provider-test.test.tsx`

- [x] Mirror blank/duplicate-name and enabled URL validation without weakening backend authority.
- [x] Render provider-specific, field-adjacent accessible errors and disable Save while any provider/reference error exists.
- [x] Prove invalid edits do not call `saveSettings` and identify the conflicting provider.

### Task 4: Verify and deliver

- [x] Run focused tests red then green, affected Rust suites, full frontend verification, workspace checks, and `git diff --check`.
- [x] Obtain independent review, commit, push, inspect both remote pipelines, and close #362 only after branch and MR pipelines are green.
