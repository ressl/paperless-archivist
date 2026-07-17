# Issue #342 Resumable Paperless Apply State Machine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ensure every Paperless document PATCH is backed by a durable, recoverable intent so an ambiguous timeout, process crash, DB failure, or lease loss can never cause a blind duplicate side effect.

**Architecture:** Add a PostgreSQL `paperless_apply_intents` state machine and a small shared `archivist-apply` orchestration crate used by human review, direct worker auto-apply, and autopilot drain. Persist a stable patch hash and ownership before HTTP, mark the request in-flight before sending, confirm or reconcile against a fresh Paperless read, and finalize local review/job state separately and idempotently. The worker periodically recovers stranded review intents; job intents are resumed by the normal lease/retry path.

**Tech Stack:** Rust 2024, SQLx/PostgreSQL 18, Reqwest/Paperless client, Axum mock servers, SHA-256, GitLab CI DB gate.

## Global Constraints

- Implement GitLab issue #342 only; #344 separately owns optimistic concurrency against newer unrelated Paperless edits.
- No distributed transaction and no blind retry of an `in_flight` intent.
- Persist intent before every PATCH, including a custom-field fallback PATCH.
- Do not print patch contents, credentials, or document content in logs.
- Preserve all existing human/worker/drain behavior, workflow-tag handling, and lease fences.
- Audit and metric transitions must be idempotent and transactionally coupled to intent state changes.

---

### Task 1: Add failing contracts for persistence and reconciliation

**Files:**
- Add: `migrations/0046_paperless_apply_intents.sql`
- Modify: `crates/archivist-db/tests/review_apply_fence.rs`
- Modify: `crates/archivist-paperless/src/lib.rs`

**Interfaces:**
- Consumes: existing `review_items.applying` fence and `PaperlessDocumentDetail`.
- Produces: RED tests for stable intent reuse, state transitions, stale-review protection, and patch-vs-document matching.

- [x] **Step 1: Write DB tests before DB helpers exist**

Cover prepare-before-send, `(source_key, patch_hash)` idempotency, ownership/attempt ID persistence, valid transition ordering, metric/audit once-only behavior, and exclusion of active intents from the old stale-`applying` reset.

- [x] **Step 2: Write pure reconciliation tests**

Cover title/content/date/correspondent/type, order-insensitive tags, custom fields, empty patches, and a single mismatching requested field.

- [x] **Step 3: Run the targeted tests and capture RED**

Run the Paperless unit tests and the ignored PostgreSQL review fence test. Expected: compile/schema failures until Tasks 2 and 3 are implemented.

### Task 2: Implement the durable PostgreSQL state machine

**Files:**
- Add: `migrations/0046_paperless_apply_intents.sql`
- Modify: `crates/archivist-db/src/lib.rs`

**Interfaces:**
- Consumes: source key, document ID, canonical patch/hash, run/job/review IDs, owner, audit metadata.
- Produces: states `prepared -> in_flight -> confirmed|reconciled|failed -> finalized` with an immutable attempt ID.

- [x] **Step 1: Add schema and constraints**

Persist attempt ID, source/source key, owner type/ID, document/run/job/review IDs, patch/hash, before image, metadata, review fallback status, timestamps, error, and state. Unique `(source_key, patch_hash)` makes retries return the same attempt.

- [x] **Step 2: Add typed CRUD/transition helpers**

Prepare/get, start, confirm, reconcile, fail, finalize, list recoverable review intents, and fetch current intent. Conditional updates make every transition idempotent.

- [x] **Step 3: Couple audit and metrics to transitions**

Write `document.patch_intent`, `document.patch_confirmed`, `document.patch_reconciled`, and `document.patch_failed`; success/failure counters increment only on the first corresponding transition.

- [x] **Step 4: Make stale review recovery intent-aware**

The legacy timeout sweep may reset `applying` only when no non-finalized apply intent exists.

### Task 3: Implement shared HTTP execution and reconciliation

**Files:**
- Add: `crates/archivist-apply/Cargo.toml`
- Add: `crates/archivist-apply/src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `crates/archivist-paperless/src/lib.rs`

**Interfaces:**
- Consumes: an apply request plus `DbPool` and `PaperlessClient`.
- Produces: confirmed/reconciled/no-op result with exactly-once HTTP behavior per durable attempt.

- [x] **Step 1: Add canonical hashing and document matching**

Serialize the typed patch deterministically, hash it with SHA-256, and compare every requested field against a fresh Paperless detail response.

- [x] **Step 2: Execute new/prepared attempts safely**

Persist `prepared`, atomically transition to `in_flight`, then send PATCH. A successful response transitions to `confirmed` before returning to caller.

- [x] **Step 3: Reconcile existing/ambiguous attempts**

An existing `in_flight` attempt performs GET only. Matching state becomes `reconciled`; mismatch becomes `failed` for operator/retry handling and is never blindly re-PATCHed. Timeout/network/5xx errors perform the same read-after-error reconciliation; an unavailable read leaves the intent in-flight for recovery.

- [x] **Step 4: Preserve custom-field fallback safely**

If Paperless definitively rejects `custom_fields`, fail that attempt and prepare a second intent for the reduced patch before the one allowed fallback PATCH.

### Task 4: Route all three callers through the shared state machine

**Files:**
- Modify: `crates/archivist-api/Cargo.toml`
- Modify: `crates/archivist-api/src/main.rs`
- Modify: `crates/archivist-worker/Cargo.toml`
- Modify: `crates/archivist-worker/src/main.rs`

**Interfaces:**
- Consumes: existing human review, direct full-auto, and autopilot drain patch builders.
- Produces: one common durable apply executor with caller-specific idempotent local finalization.

- [x] **Step 1: Human apply**

Use source key `review:<id>`, user ownership, and prior review status. Do not revert an intent-backed ambiguous/confirmed apply to an blindly retryable review state.

- [x] **Step 2: Direct worker auto-apply**

Use source key `job:<id>` and lease owner. A retry/new lease reuses confirmed/reconciled intent state and skips HTTP before calling the existing fenced `complete_job`.

- [x] **Step 3: Autopilot drain**

Use the review source key with worker ownership and the same executor. Timeout recovery uses the intent state rather than reverting blindly to pending.

- [x] **Step 4: Recover stranded review intents**

At startup and periodic recovery ticks, execute prepared intents, reconcile in-flight intents, finalize confirmed/reconciled human or drain reviews, and return failed review intents to their recorded safe status.

### Task 5: Fault-injection verification and delivery

**Files:**
- Add/modify: DB integration tests and shared apply mock-server tests
- Verify: migration, API, worker, Paperless, and workspace behavior

**Interfaces:**
- Consumes: the completed state machine and PostgreSQL 18 CI gate.
- Produces: a reviewable #342 commit with failure-boundary evidence.

- [x] **Step 1: Test every failure boundary**

Inject failure after intent, ambiguous HTTP timeout, confirmed HTTP followed by local failure, audit/metric transition replay, process restart, and lease-owner change. Assert the mock PATCH count never exceeds one for an intent.

- [x] **Step 2: Run all gates**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bash scripts/verify/migration_smoke.sh
cargo audit --deny warnings
cargo deny check
```

- [x] **Step 3: Commit, push, inspect pipeline, and document #342**

Include migration compatibility, state diagram, fault-injection results, commit SHA, and exact CI job links in the issue/MR evidence.
