# Issue #364: Bounded Provider Response Bodies Implementation Plan

**Goal:** Prevent configured OpenAI-compatible endpoints from exhausting API/worker memory with oversized or endless response bodies while preserving normal JSON, quota classification, and the structured-output compatibility retry.

### Task 1: Pin the wire-level failure modes

**Files:**
- Add: `crates/archivist-ai/tests/response_body_limits.rs`
- Modify: `crates/archivist-ai/Cargo.toml`

- [x] Add red mock-server tests for normal, oversized declared Content-Length, and oversized chunked model-list responses.
- [x] Add red mock-server tests for oversized success/error bodies in chat and vision.
- [x] Add red retry tests where the first schema error is bounded but the retry response exceeds the declared or chunked limit.
- [x] Prove a bounded 429 quota body still produces `QuotaExhausted` with a safe message.

### Task 2: Implement one bounded reader and safe diagnostics

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs`

- [x] Define one documented provider-response byte limit plus one diagnostic-snippet cap.
- [x] Reject an oversized Content-Length before reading and stop chunked/no-length streams at the same hard limit.
- [x] Keep diagnostics provider-body-content-free, cap transport metadata, and mark body/read truncation explicitly.
- [x] Preserve HTTP status-based transient/permanent classification and quota recognition without exposing raw provider bodies.

### Task 3: Route every OpenAI-compatible body through the reader

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs`

- [x] Replace unbounded model-list, chat, schema-retry, and vision success decoding.
- [x] Replace unbounded error buffering and prevent a truncated schema error from arming the compatibility retry.
- [x] Keep normal response parsing and existing provider behavior unchanged.

### Task 4: Verify and deliver

- [x] Run focused and complete archivist-ai tests, workspace tests, formatting, and Clippy with warnings denied.
- [x] Obtain an independent Critical/Important review and resolve every finding with regression coverage.
- [x] Commit/push, verify both GitLab pipelines, document evidence, and close #364 only when green.
