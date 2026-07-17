# Issue #344 Review Optimistic Concurrency Implementation Plan

**Goal:** Prevent a human or autopilot review apply from overwriting Paperless changes made after the review was created, while preserving independent edits and foreign tags.

**Semantics:** Each review stores a field-scoped baseline captured from Paperless when the review is created. Immediately before the first durable apply intent and PATCH, the shared apply layer reads the current document. A scalar field conflicts only when it changed from the baseline and is not already equal to the desired value. Tags use a three-way delta: apply the review's additions/removals to the current set, then apply workflow-tag additions/removals, preserving unrelated current tags. Recovery reuses the already persisted resolved patch and never performs a second concurrency decision.

## Task 1: Add failing field-level concurrency contracts

- [ ] Cover content, title, correspondent, document type, created date, and custom fields.
- [ ] Prove unrelated field changes do not block the requested patch.
- [ ] Prove a current value already equal to the desired value does not conflict.
- [ ] Prove baseline tags `[1,2]` plus current tags `[1,2,99]` preserve tag `99`.
- [ ] Cover requested tag removals and workflow tag additions/removals.

## Task 2: Persist field-scoped review baselines and conflicts

- [ ] Add a migration for `baseline`, `conflict_fields`, and `conflicted_at`.
- [ ] Capture a baseline for every review from the Paperless document used during creation.
- [ ] Expose conflict metadata without storing raw content in audit events.
- [ ] Add a transactional DB operation that reverts `applying` safely and records `review.apply_conflict`.
- [ ] Prove a conflict leaves review retryable and job/run/inventory waiting for review.

## Task 3: Centralize the pre-PATCH decision

- [ ] Add one shared review precondition type and resolver in `archivist-apply`.
- [ ] Run the fresh GET and field checks before creating the durable apply intent.
- [ ] Ensure no apply intent and no PATCH are produced on conflict.
- [ ] Persist the final merged patch so crash recovery remains deterministic.

## Task 4: Route human and autopilot review paths through the shared logic

- [ ] Pass the stored baseline and workflow tag operations from the human path.
- [ ] Pass the same data from the autopilot drain path.
- [ ] Convert typed conflicts into HTTP 409 for human callers.
- [ ] Record field names only and keep job/run out of successful terminal states.

## Task 5: Verify and deliver

- [ ] Run targeted unit and PostgreSQL tests.
- [ ] Run formatting, Clippy, workspace tests, audit, deny, and the full PostgreSQL gate.
- [ ] Commit, push, inspect the MR pipeline, document evidence, and close #344 only after the remote gate is green.
