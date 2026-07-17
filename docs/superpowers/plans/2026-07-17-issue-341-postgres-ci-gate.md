# Issue #341 PostgreSQL 18 CI Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every intentionally ignored `archivist-db` integration test, including the PostgreSQL 18 migration smoke test, a required GitLab MR/main pipeline gate.

**Architecture:** Keep the fast default workspace test job unchanged. Expand the existing migration-smoke script into the single local/CI database-test entry point: it starts PostgreSQL 18 through Docker when no URL is supplied, reuses GitLab's PostgreSQL 18 service when `DATABASE_URL` is supplied, runs all ignored DB tests serially, and proves/report that the complete expected test set actually ran.

**Tech Stack:** GitLab CI, Docker/PostgreSQL 18, Bash, Cargo/Rust test harness, SQLx.

## Global Constraints

- Implement GitLab issue #341 only.
- Never connect to production or shared databases; the script requires a disposable PostgreSQL 18 target.
- Do not remove `#[ignore]` from DB tests; the fast unit-test job must remain database-free.
- Run DB tests with one test thread to protect truncate-based fixtures.
- Never print `DATABASE_URL` or credentials.
- Use `scripts/verify/migration_smoke.sh` as the canonical local and CI entry point.

---

### Task 1: Capture the missing-gate contract

**Files:**
- Inspect: `.gitlab-ci.yml`
- Inspect: `scripts/verify/migration_smoke.sh`
- Inspect: `crates/archivist-db/tests/*.rs`

**Interfaces:**
- Consumes: the current unit-only `public:rust:test` pipeline and migration-only local script.
- Produces: reproducible RED evidence for the missing CI job and incomplete test runner.

- [x] **Step 1: Prove no PostgreSQL service job exists**

Run:

```bash
! rg -n 'public:rust:db-integration|postgres:18' .gitlab-ci.yml
```

Expected: success of the negated check, proving the required job is absent.

- [x] **Step 2: Inventory the intended DB tests**

Run:

```bash
grep -R -E '^[[:space:]]*#\[ignore' crates/archivist-db/tests --include='*.rs' | wc -l
```

Expected: 40 intentionally ignored integration tests.

- [x] **Step 3: Prove the existing runner covers only migration smoke**

Inspect the script and verify it invokes only `--test migration_smoke`, has no serial full-suite command, and prints no executed/skipped aggregate.

### Task 2: Turn the local migration runner into the shared DB gate

**Files:**
- Modify: `scripts/verify/migration_smoke.sh`

**Interfaces:**
- Consumes: either no URL (local Docker) or an ephemeral CI `DATABASE_URL`.
- Produces: the same PostgreSQL 18 migration and DB integration test behavior in both environments.

- [x] **Step 1: Support Docker-local and external-service modes**

If `DATABASE_URL` is absent, start the existing randomized-port PostgreSQL 18 Docker container and clean it up on exit. If it is supplied, do not invoke Docker and do not log the URL.

- [x] **Step 2: Run every ignored DB test serially**

Run:

```bash
cargo test -p archivist-db --tests --locked -- --ignored --nocapture --test-threads=1
```

Expected: migrations and all DB integration binaries execute against the disposable database; any SQL, migration, connection, or assertion failure fails the script.

- [x] **Step 3: Enforce and report the test count**

Capture Cargo's output, sum passed and ignored counts, and compare passed tests with the source inventory. Print `executed`, `skipped`, and `expected`; fail if no test ran, if any intended test was ignored, or if the totals differ.

### Task 3: Add the required PostgreSQL 18 GitLab job

**Files:**
- Modify: `.gitlab-ci.yml`

**Interfaces:**
- Consumes: GitLab MR/main workflow and the shared verification script.
- Produces: a required test-stage job with an isolated PostgreSQL 18 service.

- [x] **Step 1: Define the ephemeral service and test-only variables**

Add `public:rust:db-integration` with a `postgres:18` service alias, isolated test database/user/password variables, and an internal service-host `DATABASE_URL`. Values are disposable CI credentials and the script never prints them.

- [x] **Step 2: Call the same local verification entry point**

The job script must contain only the normal environment diagnostics plus:

```bash
bash scripts/verify/migration_smoke.sh
```

Keep `public:rust:test` unchanged as the fast database-free job.

- [x] **Step 3: Validate CI syntax and resolved jobs**

Run:

```bash
glab ci lint --include-jobs
```

Expected: valid CI configuration containing the new DB integration job.

### Task 4: Verify locally, commit, and obtain the remote gate

**Files:**
- Verify: `.gitlab-ci.yml`
- Verify: `scripts/verify/migration_smoke.sh`

**Interfaces:**
- Consumes: local Docker and the GitLab PostgreSQL service definition.
- Produces: a reviewable #341 commit and a green required pipeline job.

- [x] **Step 1: Run the canonical local PostgreSQL 18 suite**

Run:

```bash
bash scripts/verify/migration_smoke.sh
```

Expected: PostgreSQL 18 starts ephemerally; exactly 40 tests execute, zero are skipped, and all pass.

- [x] **Step 2: Run script and repository checks**

Run:

```bash
bash -n scripts/verify/migration_smoke.sh
cargo fmt --all -- --check
git diff --check
```

Expected: all checks pass.

- [x] **Step 3: Commit and push #341**

Run:

```bash
git add .gitlab-ci.yml scripts/verify/migration_smoke.sh
git commit -m "ci: require PostgreSQL integration tests"
git push origin codex/milestone-51
```

- [x] **Step 4: Verify the GitLab job and document evidence**

Inspect the newest MR pipeline, confirm `public:rust:db-integration` reports `executed=40 skipped=0 expected=40`, then add commit/job/pipeline links and verification results to #341.
