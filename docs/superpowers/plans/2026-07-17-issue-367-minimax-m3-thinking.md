# Issue #367: MiniMax M3 Thinking Contract Plan

**Goal:** Send MiniMax M3's exact `thinking_mode` wire contract and prevent inline or parser-separated reasoning from leaking into application result text.

### Task 1: Pin payload behavior with failing tests

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs`

- [x] Cover the exact M3 model identity and all four `ReasoningEffort` mappings.
- [x] Reject an unresolved M3 effort instead of silently omitting `thinking_mode`.
- [x] Prove other namespaces, lookalike model names, and ordinary OpenAI-compatible models remain unchanged.

### Task 2: Pin defensive response parsing with failing tests

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs`

- [x] Cover `<think>` and `<mm:think>` blocks, mixed/multiple blocks, and unterminated tails.
- [x] Keep `reasoning_content` in the raw response while returning only visible answer content.
- [x] Return an actionable contract/configuration error for reasoning-only responses.

### Task 3: Implement the smallest model-aware contract

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs`

- [x] Add the exact MiniMax M3 capability check and `chat_template_kwargs.thinking_mode` mapping.
- [x] Make unresolved M3 requests fail before network I/O.
- [x] Replace the single-tag fallback with a conservative multi-tag state machine.

### Task 4: Verify and deliver

- [x] Run focused red/green tests, `cargo test -p archivist-ai`, formatting, lint, and workspace checks.
- [x] Obtain an independent Critical/Important review and resolve every finding.
- [x] Commit/push, verify both GitLab pipelines, document evidence, and close #367 only when green.
