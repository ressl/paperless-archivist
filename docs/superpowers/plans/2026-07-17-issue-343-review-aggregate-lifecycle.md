# Issue #343 Review Aggregate Lifecycle Implementation Plan

**Goal:** Keep a review-backed job, run, and inventory open until every sibling review item reaches a terminal state, then finalize the aggregate exactly once.

**Semantics:** `applied` and `rejected` are terminal review states. `pending`, `approved`, `edited`, and `applying` remain nonterminal. A mixed result succeeds the job because at least one accepted field was applied; rejected fields remain explicitly audited. An all-rejected aggregate cancels the job and rejects the run/stage after the last decision only.

## Task 1: Add failing PostgreSQL aggregate contracts

- [ ] Seed a run/job/inventory with three sibling review items.
- [ ] Prove the first applied or rejected item cannot finalize job, run, or inventory.
- [ ] Cover approve/approve, approve/reject, reject-first, all-rejected, and concurrent final decisions.
- [ ] Assert the next stage remains unclaimable until the aggregate is terminal.

## Task 2: Centralize atomic aggregate finalization

- [ ] Lock the shared job row before evaluating sibling state.
- [ ] Count terminal/applied/rejected siblings in the same transaction.
- [ ] Keep `waiting_review` and `needs_review=true` while any sibling is nonterminal.
- [ ] On the last terminal review, transition the job and downstream run/inventory exactly once.
- [ ] Emit one aggregate-finalized audit event from the winning conditional transition.

## Task 3: Route every decision path through the aggregate

- [ ] Human rejection must no longer reject the run immediately.
- [ ] Human apply and autopilot apply must not succeed the job after one item.
- [ ] Preserve per-item `review.applied` / `review.rejected` audit events.
- [ ] Keep the #342 apply-intent finalization separate and idempotent.

## Task 4: Verify and deliver

- [ ] Run the targeted PostgreSQL tests with PostgreSQL 18.
- [ ] Run formatting, Clippy, workspace tests, audit, deny, and the full DB gate.
- [ ] Commit, push, inspect the MR pipeline, document evidence, and close #343 only after the remote gate is green.
