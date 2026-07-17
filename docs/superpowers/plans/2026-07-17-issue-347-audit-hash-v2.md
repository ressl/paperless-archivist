# Issue #347 Audit Hash v2 Implementation Plan

**Goal:** Bind audit origin metadata into every new event hash without invalidating the existing audit history, and make the integrity report explicit about unhashed, v1, and v2 coverage.

**Compatibility contract:** Migration `0048` adds a nullable `hash_version`. Existing hashed rows are labelled v1 without changing their payload, linkage, or hash; pre-chain rows remain unversioned and unhashed. New rows are written explicitly as v2. Verification selects the canonical payload by each row's version and fails closed on unknown versions.

## Task 1: Add failing hash-version tests

- [ ] Add a deterministic canonical-payload test for v1 and v2.
- [ ] Prove v2 changes when `source_ip` or `user_agent` changes, including `Some` versus `null`.
- [ ] Prove new database events are explicitly version 2.
- [ ] Add PostgreSQL tamper tests for each origin field.
- [ ] Add a mixed unhashed/v1/v2 chain test with coverage assertions.

## Task 2: Introduce the versioned storage and hash contract

- [ ] Add migration `0048_audit_hash_v2.sql` without rewriting existing payloads or hashes.
- [ ] Preserve the byte-for-byte v1 canonical payload implementation.
- [ ] Add the v2 discriminator plus `source_ip` and `user_agent` to the canonical payload, including nulls.
- [ ] Persist version 2 atomically with each new hash and chain link.
- [ ] Reject unsupported versions as an integrity failure rather than a server error.

## Task 3: Expose forensic coverage

- [ ] Report unhashed legacy, hashed v1, and hashed v2 event counts separately.
- [ ] Include `hash_version` in audit records and CSV export.
- [ ] Update OpenAPI, generated schema, frontend API type, and the audit integrity summary.
- [ ] Keep existing fields backward-compatible for API consumers.

## Task 4: Verify and deliver

- [ ] Run focused unit, PostgreSQL tamper, mixed-chain, API, and frontend tests.
- [ ] Run formatting, Clippy, workspace tests, audit, deny, frontend gates, and all PostgreSQL integration tests.
- [ ] Obtain independent review, commit, push, inspect the MR pipeline, document evidence, and close #347 only after the remote gate is green.
