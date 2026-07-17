# Issue #350 User Identity Namespace Implementation Plan

**Goal:** Make username and non-empty email resolution case-insensitive, unambiguous, and database-enforced across local login, OIDC linking/provisioning, and the Paperless login bridge.

**Identity contract:** PostgreSQL owns one normalization function: trim surrounding whitespace, apply database lowercase, and map an empty result to no identity. Every username and non-empty email claims a key in one shared namespace. A primary key permits one owner per normalized identity; a user's own equal username/email may share that claim. The synchronization trigger and namespace primary key remain the final concurrent-write invariant; retrying provisioners use savepoints instead of introducing a lock order above user rows.

## Task 1: Prove current ambiguity and migration behavior

- [x] Add PostgreSQL tests for username/email case variants and both cross-column collision directions.
- [x] Add a parallel-create test using independent connections and assert exactly one namespace owner.
- [x] Prove login resolves case/whitespace variants through a structurally unique namespace key.
- [x] Cover OIDC username/email linking and suffix allocation against cross-column claims.
- [x] Cover the Paperless bridge's instance-scoped stable user-ID mapping, disabled-account/token-rotation behavior, lossy local-name collisions, and concurrent get-or-create behavior.
- [x] Stage a schema at migration 48 with legacy collisions; assert migration 49 fails with actionable detail and leaves both accounts unchanged.

## Task 2: Enforce one canonical identity namespace

- [x] Add the PostgreSQL normalization function, namespace table, preflight, backfill, constraints, and synchronization trigger.
- [x] Permit blank legacy email only by converting it to `NULL`; reject blank usernames and future blank emails.
- [x] Use the namespace primary key plus conflict-safe savepoint retries to make concurrent writes race-safe.
- [x] Route generic login and kind-aware lookups through namespace claims so each query returns at most one account.
- [x] Make OIDC link/update/provision paths use kind-aware namespace claims and reject a two-account link instead of choosing arbitrarily.
- [x] Make Paperless bridge provisioning recover only the same explicit provider/subject mapping after a concurrent create; never adopt a prefixed local account.
- [x] Map identity conflicts to stable API conflict responses without leaking account details.
- [x] Document normalization, migration preflight, and operator remediation.

## Task 3: Verify compatibility and delivery

- [x] Run focused identity/migration tests and the full PostgreSQL migration smoke suite.
- [x] Run formatting, Clippy, workspace tests, audit, deny, and frontend gates.
- [ ] Obtain independent review, commit, push, inspect the MR pipeline, document evidence, and close #350 only after the remote gate is green.
