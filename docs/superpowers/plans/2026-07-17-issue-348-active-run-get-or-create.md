# Issue #348 Active Run Get-or-create Implementation Plan

**Goal:** Make concurrent single and bulk run creation for the same Paperless document converge on one active run without exposing an expected unique-index race as a server error.

**Concurrency contract:** Every creation transaction takes a transaction-scoped PostgreSQL advisory lock keyed by Paperless document id, then uses the active-run partial unique index through `INSERT ... ON CONFLICT DO NOTHING` as the linearization point. The lock scope is one document, so unrelated documents remain independent. Bulk callers acquire locks in ascending de-duplicated document order and resolve every unique-index conflict before the first audit append.

## Task 1: Add failing concurrency coverage

- [x] Add a deterministic PostgreSQL barrier test using independent pools.
- [x] Race single creation against single creation and assert both return the same run id.
- [x] Race single creation against bulk creation and assert identical get-or-create behavior.
- [x] Assert exactly one run, one expected job set, and one `run.created` audit event.
- [x] Prove a locked document does not prevent another document from being created.
- [x] Cover active-to-terminal, terminal-to-active, and Audit/Row-lock races.
- [x] Prove startup reactivators select at most one terminal run per document.

## Task 2: Serialize per-document creation

- [x] Take a transaction-scoped advisory lock before each document get-or-create.
- [x] Use a conflict-safe partial-index insert and retry a disappearing conflict.
- [x] Preserve the existing active-status definition and unique partial index as the final invariant.
- [x] Normalize bulk document ids into deterministic ascending order before taking locks.
- [x] Resolve all batch get-or-create conflicts before the first audit append.
- [x] Coordinate terminal-run reactivation with the same document lock.
- [x] Keep new-run jobs, inventory state, and audit creation in the same transaction.

## Task 3: Verify compatibility and delivery

- [x] Run the focused concurrency tests and the full PostgreSQL migration smoke suite.
- [x] Run formatting, Clippy, workspace tests, audit, deny, and frontend gates.
- [x] Obtain independent review, commit, push, inspect the MR pipeline, document evidence, and close #348 only after the remote gate is green.
