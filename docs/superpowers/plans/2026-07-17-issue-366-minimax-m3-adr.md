# Issue #366: MiniMax M3 Integration ADR Plan

**Goal:** Freeze one implementation contract for SGLang-served MiniMax M3 so the provider, thinking, consumer, test, and operations tickets cannot diverge.

### Task 1: Pin authoritative evidence

**Files:**
- Modify: `docs/ARCHITECTURE_DECISIONS.md`

- [x] Compare the official MiniMax M3 model card and chat template with the target model.
- [x] Record the public target-model revision and the live pinned SGLang image/runtime revision without private endpoints.
- [x] Compare the target checkpoint capabilities and validated limits with the official SGLang M3 cookbook.

### Task 2: Decide scope and wire semantics

**Files:**
- Modify: `docs/ARCHITECTURE_DECISIONS.md`

- [ ] Select text-first scope and enumerate every text/control-plane consumer.
- [ ] Keep SGLang under `openai_compatible` and pin the exact model identity.
- [ ] Define `ReasoningEffort` to `thinking_mode` mapping, omission behavior, parser fallback, and failure semantics.
- [ ] Define the future vision gate, OCR dependencies, risks, alternatives, and migration behavior.

### Task 3: Supersede the M2.7 design precisely

**Files:**
- Modify: `docs/superpowers/specs/2026-07-07-mineru-sglang-providers-design.md`
- Modify: `docs/FEATURE_REFERENCE.md`

- [ ] Mark the old design as partially superseded while retaining its still-valid protocol and MinerU decisions.
- [ ] Point the feature reference at the ADR and distinguish current text scope from optional vision capability.

### Task 4: Verify and deliver

- [ ] Check source links, Markdown/diff hygiene, model/template/runtime fingerprints, and repository lint.
- [ ] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify both GitLab pipelines, document evidence, and close #366 only when green.
