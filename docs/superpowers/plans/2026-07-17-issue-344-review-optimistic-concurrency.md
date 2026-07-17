# Issue #344 Review Optimistic Concurrency Implementation Plan

**Goal:** Prevent a human or autopilot review apply from overwriting Paperless changes made after the review was created, while preserving independent edits and foreign tags.

**Semantics:** Each review stores fingerprints/IDs for every patchable field captured from Paperless when the review is created, including fields a reviewer may add while editing. Immediately before the first durable apply intent and PATCH, the shared apply layer reads the current document. A scalar field conflicts only when it changed from the baseline and is not already equal to the desired value. Tags use a three-way delta: apply the review's additions/removals to the current set, then apply workflow-tag additions/removals, preserving unrelated current tags. Recovery reuses the already persisted resolved patch and never performs a second concurrency decision.

## Task 1: Add failing field-level concurrency contracts

- [x] Cover content, title, correspondent, document type, created date, and custom fields.
- [x] Prove unrelated field changes do not block the requested patch.
- [x] Prove a current value already equal to the desired value does not conflict.
- [x] Prove baseline tags `[1,2]` plus current tags `[1,2,99]` preserve tag `99`.
- [x] Cover requested tag removals and workflow tag additions/removals.

## Task 2: Persist field-scoped review baselines and conflicts

- [x] Add a migration for `baseline`, `conflict_fields`, and `conflicted_at`.
- [x] Capture a baseline for every review from the Paperless document used during creation.
- [x] Expose conflict metadata without storing raw content in audit events.
- [x] Add a transactional DB operation that reverts `applying` safely and records `review.apply_conflict`.
- [x] Prove a conflict leaves review retryable and job/run/inventory waiting for review.

## Task 3: Centralize the pre-PATCH decision

- [x] Add one shared review precondition type and resolver in `archivist-apply`.
- [x] Run the fresh GET and field checks before creating the durable apply intent.
- [x] Ensure no apply intent and no PATCH are produced on conflict.
- [x] Persist the final merged patch so crash recovery remains deterministic.

## Task 4: Route human and autopilot review paths through the shared logic

- [x] Pass the stored baseline and workflow tag operations from the human path.
- [x] Pass the same data from the autopilot drain path.
- [x] Convert typed conflicts into HTTP 409 for human callers.
- [x] Record field names only and keep job/run out of successful terminal states.

## Task 5: Verify and deliver

- [x] Run targeted unit and PostgreSQL tests.
- [x] Run formatting, Clippy, workspace tests, audit, deny, and the full PostgreSQL gate.
- [x] Commit, push, inspect the MR pipeline, document evidence, and close #344 only after the remote gate is green.
