# Issue #345 Metadata Contract Diagnostics Implementation Plan

**Goal:** Distinguish legitimate metadata omissions from malformed model responses, retain safe field-level evidence, and ensure contract violations retry or fail instead of completing successfully.

**Semantics:** Parsing always returns a suggestion plus value-free diagnostics. Empty objects and known keys set to `null` are legitimate omissions. No JSON, non-object JSON, unknown keys, and known keys with invalid types are contract violations. Mixed responses retain successfully decoded fields in the AI artifact, but the worker does not partially apply them while any contract violation remains. Contract violations are typed retryable processing failures and become visible in the job error after the retry budget is exhausted.

## Task 1: Add failing parser contracts

- [x] Cover no JSON, array/non-object JSON, unknown keys, invalid known-field types, mixed responses, and `{}`.
- [x] Prove mixed responses retain valid decoded fields and name invalid fields.
- [x] Prove diagnostics never contain raw invalid values or response content.
- [x] Prove known `null` values remain legitimate omissions.

## Task 2: Implement structured parser diagnostics

- [x] Replace silent per-field `.ok()` drops with explicit decoded/null/invalid field tracking.
- [x] Add a stable parse status and envelope error kind.
- [x] Count unknown keys without persisting their potentially sensitive names.
- [x] Add a typed metadata-contract error containing only safe diagnostics.

## Task 3: Route worker outcomes safely

- [x] Remove `unwrap_or_else(Default::default)` from the metadata worker.
- [x] Persist the raw provider artifact according to storage policy plus normalized safe diagnostics before returning a contract error.
- [x] Classify the typed contract error as retryable and prove it cannot reach model-omitted success.
- [x] Include parse diagnostics in successful omission job results.

## Task 4: Verify and deliver

- [x] Run targeted parser and worker-routing tests.
- [x] Run formatting, Clippy, workspace tests, audit, deny, frontend gates, and PostgreSQL integration tests.
- [x] Commit, push, inspect the MR pipeline, document evidence, and close #345 only after the remote gate is green.
