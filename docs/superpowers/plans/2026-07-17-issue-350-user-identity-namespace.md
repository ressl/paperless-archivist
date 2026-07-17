# Issue #350 User Identity Namespace Implementation Plan

**Goal:** Make username and non-empty email resolution case-insensitive, unambiguous, and database-enforced across local login, OIDC linking/provisioning, and the Paperless login bridge.

**Identity contract:** PostgreSQL owns one normalization function: trim surrounding whitespace, apply database lowercase, and map an empty result to no identity. Every username and non-empty email claims a key in one shared namespace. A primary key permits one owner per normalized identity; a user's own equal username/email may share that claim. Writers are serialized by a transaction advisory lock and the namespace trigger remains the final invariant.

## Task 1: Prove current ambiguity and migration behavior

- [ ] Add PostgreSQL tests for username/email case variants and both cross-column collision directions.
- [ ] Add a parallel-create test using independent connections and assert exactly one namespace owner.
- [ ] Prove login resolves case/whitespace variants through a structurally unique namespace key.
- [ ] Cover OIDC username/email linking and suffix allocation against cross-column claims.
- [ ] Cover the Paperless bridge's username-only lookup and concurrent get-or-create behavior.
- [ ] Stage a schema at migration 48 with legacy collisions; assert migration 49 fails with actionable detail and leaves both accounts unchanged.

## Task 2: Enforce one canonical identity namespace

- [ ] Add the PostgreSQL normalization function, namespace table, preflight, backfill, constraints, and synchronization trigger.
- [ ] Permit blank legacy email only by converting it to `NULL`; reject blank usernames and future blank emails.
- [ ] Use the namespace primary key plus the transaction lock to make concurrent writes race-safe.
- [ ] Route generic login and username-only lookups through namespace claims so each query returns at most one account.
- [ ] Make OIDC link/update/provision paths use kind-aware namespace claims and reject a two-account link instead of choosing arbitrarily.
- [ ] Make Paperless bridge provisioning recover the same username owner after a concurrent create without ever matching an email claim.
- [ ] Map identity conflicts to stable API conflict responses without leaking account details.
- [ ] Document normalization, migration preflight, and operator remediation.

## Task 3: Verify compatibility and delivery

- [ ] Run focused identity/migration tests and the full PostgreSQL migration smoke suite.
- [ ] Run formatting, Clippy, workspace tests, audit, deny, and frontend gates.
- [ ] Obtain independent review, commit, push, inspect the MR pipeline, document evidence, and close #350 only after the remote gate is green.
