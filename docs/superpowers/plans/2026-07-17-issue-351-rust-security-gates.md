# Issue #351 Rust Security Gates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the RustSec-blocked versions of `anyhow` and `quinn-proto`, eliminate the active yanked `spin` path, and restore the `cargo audit` and `cargo deny` gates.

**Architecture:** Keep the dependency change minimal. Raise the direct workspace `anyhow` floor, update the transitive QUIC protocol package in `Cargo.lock`, and remove Axum's unused server-side `multipart` feature; MinerU multipart uploads continue to use Reqwest and are unaffected.

**Tech Stack:** Cargo workspace, Rust 2024, Axum 0.8, Reqwest 0.13, cargo-audit, cargo-deny.

## Global Constraints

- Implement GitLab issue #351 only.
- Do not ignore RustSec advisories or relax `deny.toml`.
- Keep Axum `macros`; remove only its unused `multipart` feature.
- Keep Reqwest `multipart`; it is required by the MinerU client.
- Preserve the committed `Cargo.lock` and `--locked` CI workflow.
- Treat `cargo audit` and `cargo deny check` as the failing security-contract tests.

---

### Task 1: Capture the failing security contract

**Files:**
- Inspect: `Cargo.toml`
- Inspect: `Cargo.lock`
- Inspect: `deny.toml`

**Interfaces:**
- Consumes: the current workspace dependency graph at `c892d12`.
- Produces: a reproducible RED state and exact dependency paths for the following tasks.

- [ ] **Step 1: Run the RustSec gate and verify the expected failure**

Run:

```bash
cargo audit
```

Expected: exit 1 with `RUSTSEC-2026-0185` for `quinn-proto 0.11.14` and `RUSTSEC-2026-0190` for `anyhow 1.0.102`.

- [ ] **Step 2: Run the dependency policy gate and verify the expected failure**

Run:

```bash
cargo deny check
```

Expected: exit 1 for `anyhow 1.0.102` and active yanked `spin 0.9.8` through `multer -> axum`.

- [ ] **Step 3: Prove the server multipart feature is unused**

Run:

```bash
rg -n '\bMultipart\b|multipart' crates Cargo.toml
cargo tree -i spin --locked
```

Expected: no `axum::extract::Multipart` use; MinerU multipart appears only under Reqwest call sites/tests; the active `spin` path is `multer -> axum -> archivist-api`.

### Task 2: Upgrade the vulnerable dependency versions

**Files:**
- Modify: `Cargo.toml:25`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: workspace crates using `anyhow.workspace = true` and Reqwest's transitive QUIC packages.
- Produces: `anyhow >=1.0.103` and `quinn-proto >=0.11.15` in the locked graph.

- [ ] **Step 1: Raise the direct anyhow floor**

Change the workspace dependency to:

```toml
anyhow = "1.0.103"
```

- [ ] **Step 2: Update only the affected locked packages**

Run:

```bash
cargo update -p anyhow --precise 1.0.103
cargo update -p quinn-proto --precise 0.11.15
```

Expected: `Cargo.lock` contains `anyhow 1.0.103` and `quinn-proto 0.11.15` without unrelated major updates.

- [ ] **Step 3: Verify the vulnerability gate turns green**

Run:

```bash
cargo audit
```

Expected: exit 0 with no vulnerabilities. Yanked packages may be reported as non-fatal lockfile warnings; Task 3 removes the active policy violation.

### Task 3: Remove the unused active yanked dependency path

**Files:**
- Modify: `Cargo.toml:28`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: Axum routing and macro support.
- Produces: Axum without its unused server-side multipart extractor; Reqwest multipart remains enabled for MinerU.

- [ ] **Step 1: Remove only Axum's multipart feature**

Change the workspace dependency to:

```toml
axum = { version = "0.8", features = ["macros"] }
```

- [ ] **Step 2: Re-resolve the lockfile**

Run:

```bash
cargo update -p axum
```

Expected: the active `multer -> spin` path disappears. Cargo may retain dormant optional packages in `Cargo.lock`; policy is evaluated against the active target graph.

- [ ] **Step 3: Verify the dependency policy turns green**

Run:

```bash
cargo tree -i spin --locked
cargo deny check
```

Expected: `cargo tree` prints no active reverse dependency for the host target, and `cargo deny check` exits 0 without relaxing `deny.toml`.

- [ ] **Step 4: Verify MinerU multipart remains intact**

Run:

```bash
cargo test -p archivist-ai --test wire_contracts mineru_vision_multipart_roundtrip --locked -- --exact
```

Expected: PASS, proving Reqwest multipart upload behavior is unchanged.

### Task 4: Run the full Rust verification and commit #351

**Files:**
- Verify: `Cargo.toml`
- Verify: `Cargo.lock`

**Interfaces:**
- Consumes: the final dependency graph from Tasks 2 and 3.
- Produces: a reviewable #351 commit with all Rust gates green.

- [ ] **Step 1: Run formatting and lint gates**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: both commands exit 0.

- [ ] **Step 2: Run all default workspace tests**

Run:

```bash
cargo test --workspace --locked
```

Expected: all runnable tests pass; PostgreSQL tests remain explicitly ignored until issue #341 installs the CI database job.

- [ ] **Step 3: Re-run both security gates**

Run:

```bash
cargo audit
cargo deny check
```

Expected: both commands exit 0.

- [ ] **Step 4: Inspect and commit only #351 files**

Run:

```bash
git diff --check
git diff -- Cargo.toml Cargo.lock
git add Cargo.toml Cargo.lock
git commit -m "fix(deps): restore Rust security gates"
```

Expected: one commit containing the minimal dependency changes, with no OCR worktree content; this plan remains in its preceding documentation commit.

### Task 5: Satisfy the stricter shared GitLab audit gate

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Add: `crates/archivist-sqlx/Cargo.toml`
- Add: `crates/archivist-sqlx/src/lib.rs`

**Interfaces:**
- Consumes: pipeline 9521, job 62808, whose group blueprint runs `cargo audit --deny warnings`.
- Produces: a lockfile with no dormant SQLite/MySQL facade packages and therefore no yanked `spin` package at all.

- [ ] **Step 1: Capture the stricter remote RED state**

Inspect job 62808.

Expected: the local `cargo audit` exits 0, but the shared job exits 1 because `--deny warnings` promotes the dormant `sqlx-sqlite -> flume -> spin 0.9.8` lockfile warning to an error.

- [ ] **Step 2: Replace the multi-driver facade with an internal PostgreSQL-only facade**

Add the non-published `archivist-sqlx` workspace crate, whose library name remains `sqlx`. It depends on exact `sqlx-core = 0.9.0` and `sqlx-postgres = 0.9.0` and exposes only the API subset already used by the application. Keep the same Tokio, Rustls, migration, Chrono, UUID, and JSON capabilities. Exact pins are required because SQLx Core's API is semver-exempt.

- [ ] **Step 3: Preserve the existing application-facing facade contract**

Re-export query helpers/traits from `sqlx-core` and `PgPool`, `PgRow`, `PgPoolOptions`, and `Postgres` through the PostgreSQL driver. Existing call sites stay unchanged; SQL, transaction behavior, pool configuration, and migration logic are not modified.

- [ ] **Step 4: Prove lockfile and runtime compatibility**

Run:

```bash
cargo audit --deny warnings
cargo deny check
cargo test --workspace --locked
bash scripts/verify/migration_smoke.sh
```

Expected: no `spin`, `flume`, `sqlx-sqlite`, `sqlx-mysql`, or SQLx macro packages remain in `Cargo.lock`; all unit, wire, migration, and 40 PostgreSQL integration tests pass.
