# Issue #346 Last Enabled Administrator Invariant Implementation Plan

**Goal:** Prevent every manual or OIDC-driven mutation from leaving Paperless Archivist without an enabled administrator, including concurrent demotions, while returning a safe domain conflict and preserving an audit trail.

**Concurrency contract:** Every operation that can remove an enabled administrator acquires the same transaction-scoped PostgreSQL advisory lock before reading the current enabled-admin set. The invariant check, accepted mutation, session revocation, and audit event remain in that transaction. A rejected mutation writes a failure audit event, commits it without changing roles/enabled/session state, and then returns a typed domain error.

## Task 1: Add failing invariant tests

- [ ] Cover self- and foreign disable/demotion of the sole enabled administrator.
- [ ] Prove two enabled administrators still allow one disable and one demotion.
- [ ] Run two demotions concurrently and prove exactly one succeeds.
- [ ] Prove rejected disable does not revoke sessions and emits a safe failure audit.
- [ ] Cover API mapping of the typed invariant error to HTTP 409.

## Task 2: Centralize the database invariant

- [ ] Add a stable typed `LastEnabledAdminError` domain error.
- [ ] Add one transaction-scoped advisory lock shared by manual and OIDC role mutations.
- [ ] Check target enabled/admin state and the remaining enabled-admin set under the lock.
- [ ] Reuse the existing role replacement helper instead of duplicating mutations.
- [ ] Preserve the existing OIDC behavior that keeps the Admin role on the last enabled admin.

## Task 3: Make rejection observable and API-safe

- [ ] Audit rejected enabled/role changes with actor, target, attempted state, failure outcome, and no secret/session payload.
- [ ] Commit rejection audits without applying the attempted mutation or revoking sessions.
- [ ] Map the typed error to a stable 409 response without exposing database internals.

## Task 4: Verify and deliver

- [ ] Run focused DB/API/OIDC tests and the PostgreSQL concurrency test.
- [ ] Run formatting, Clippy, workspace tests, audit, deny, frontend gates, and all PostgreSQL integration tests.
- [ ] Commit, push, inspect the MR pipeline, document evidence, and close #346 only after the remote gate is green.
