# Issue #348 Active Run Get-or-create Implementation Plan

**Goal:** Make concurrent single and bulk run creation for the same Paperless document converge on one active run without exposing an expected unique-index race as a server error.

**Concurrency contract:** Every creation transaction takes a transaction-scoped PostgreSQL advisory lock keyed by Paperless document id before checking for an active run. The lock scope is one document, so unrelated documents remain independent. Bulk callers acquire locks in ascending de-duplicated document order to avoid cross-batch lock-order inversions.

## Task 1: Add failing concurrency coverage

- [ ] Add a deterministic PostgreSQL barrier test using independent pools.
- [ ] Race single creation against single creation and assert both return the same run id.
- [ ] Race single creation against bulk creation and assert identical get-or-create behavior.
- [ ] Assert exactly one run, one expected job set, and one `run.created` audit event.
- [ ] Prove a locked document does not prevent another document from being created.

## Task 2: Serialize per-document creation

- [ ] Take a transaction-scoped advisory lock for the document before the active-run lookup.
- [ ] Preserve the existing active-status definition and unique partial index as the final invariant.
- [ ] Normalize bulk document ids into deterministic ascending order before taking locks.
- [ ] Keep new-run jobs, inventory state, and audit creation in the same transaction.

## Task 3: Verify compatibility and delivery

- [ ] Run the focused concurrency tests and the full PostgreSQL migration smoke suite.
- [ ] Run formatting, Clippy, workspace tests, audit, deny, and frontend gates.
- [ ] Obtain independent review, commit, push, inspect the MR pipeline, document evidence, and close #348 only after the remote gate is green.
