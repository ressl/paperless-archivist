# Inventory Status Reconciliation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-driven-development (recommended) or executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep inventory stage statuses, dashboard counts, automatic selection, Paperless completion tags, and Paperless modified timestamps consistent, then repair the live v1.18.0 data without using a GPU.

**Architecture:** Normalize authoritative completion evidence at the inventory upsert boundary and backfill it in migration 0052. Treat rejected reviews as terminal, extend the existing Paperless reconciliation endpoint with inventory-status evidence, and share timestamp parsing across both sync implementations.

**Tech Stack:** Rust, SQLx, PostgreSQL 18, Axum, Tokio, Paperless-ngx REST API, GitLab CI, Kubernetes/Argo CD.

## Global Constraints

- Do not start or scale SGLang, MiniMax M3, or any GPU workload.
- Preserve every existing job, run, review, artifact, and audit record.
- Paperless tags remain the external source of truth; never fake a Paperless tag only in PostgreSQL.
- Use a failing test before every production-code behavior change.
- Run live corrections through migrations or the authenticated application endpoint, not ad-hoc unaudited SQL.

---

### Task 1: Inventory tag ratchets and modified timestamps

**Files:**
- Modify: `crates/archivist-db/src/lib.rs`
- Modify: `crates/archivist-db/tests/sync_noop_upserts.rs`
- Modify: `crates/archivist-api/src/main.rs`
- Modify: `crates/archivist-worker/src/main.rs`
- Create: `migrations/0052_reconcile_inventory_completion_status.sql`
- Modify: `crates/archivist-db/tests/migration_smoke.rs`

**Interfaces:**
- Produces: `parse_paperless_modified_at(Option<&str>) -> Option<DateTime<Utc>>`.
- Preserves: `upsert_inventory_item(&mut Transaction<'_, Postgres>, &InventoryUpsert) -> Result<()>`.

- [ ] **Step 1: Add failing integration tests for completion evidence**

Extend `inventory_upsert_guard_preserves_status_ratchet_semantics` with cases
that seed drifted OCR and metadata states, apply a metadata tag or full tag,
and require:

```rust
assert_eq!(ocr_status, "succeeded");
assert_eq!(metadata_status, "succeeded");
assert!(complete);
```

- [ ] **Step 2: Add failing timestamp tests**

Require RFC3339 parsing to retain UTC instants and require an upsert with
`paperless_modified_at=None` to preserve a previously stored timestamp without
changing `xmin` or `last_seen_at`.

- [ ] **Step 3: Run RED tests**

```bash
DATABASE_URL="$DISPOSABLE_DATABASE_URL" cargo test -p archivist-db --test sync_noop_upserts -- --ignored --test-threads=1
cargo test -p archivist-db paperless_modified --lib
```

Expected: status assertions fail because metadata/full tags are not ratcheted,
and timestamp tests fail because no shared parser/preservation exists.

- [ ] **Step 4: Implement the minimal upsert and parser changes**

Compute insert statuses as:

```rust
let ocr_complete = item.has_ocr_completion_tag || item.has_full_completion_tag;
let metadata_complete = item.has_tagging_completion_tag || item.has_full_completion_tag;
```

Add `metadata_status` to the insert/update/no-op tuples. Use
`coalesce(excluded.paperless_modified_at, document_inventory.paperless_modified_at)`
in both update and comparison tuples. Replace the API-local parser and worker
`None` with `archivist_db::parse_paperless_modified_at(document.modified.as_deref())`.

- [ ] **Step 5: Add migration 0052**

Use one transactional migration statement to ratchet rows with stage/full tags:

```sql
update document_inventory
set ocr_status = case when has_ocr_completion_tag or has_full_completion_tag then 'succeeded' else ocr_status end,
    metadata_status = case when has_tagging_completion_tag or has_full_completion_tag then 'succeeded' else metadata_status end,
    complete = has_full_completion_tag,
    updated_at = now()
where (has_ocr_completion_tag or has_full_completion_tag) and ocr_status <> 'succeeded'
   or (has_tagging_completion_tag or has_full_completion_tag) and metadata_status <> 'succeeded'
   or complete is distinct from has_full_completion_tag;
```

Add `0052_reconcile_inventory_completion_status.sql` to the migration smoke
expectation.

- [ ] **Step 6: Run GREEN tests and commit**

```bash
DATABASE_URL="$DISPOSABLE_DATABASE_URL" cargo test -p archivist-db --test sync_noop_upserts -- --ignored --test-threads=1
cargo test -p archivist-db --lib
cargo test -p archivist-api --bin archivist-api
cargo test -p archivist-worker --bin archivist-worker
git add crates/archivist-db crates/archivist-api/src/main.rs crates/archivist-worker/src/main.rs migrations/0052_reconcile_inventory_completion_status.sql
git commit -m "fix(db): reconcile inventory completion evidence"
```

### Task 2: Terminal stage semantics and dashboard counts

**Files:**
- Modify: `crates/archivist-db/src/lib.rs`
- Create: `crates/archivist-db/tests/dashboard_stage_status.rs`

**Interfaces:**
- Consumes: `DashboardStageStatus` and `get_dashboard_stats`.
- Produces: consistent terminal-state predicate for `succeeded`, `skipped`, `not_needed`, and `rejected`.

- [ ] **Step 1: Add failing selector unit test**

Add a test where metadata is `rejected`, no completion tag exists, and the
enabled stage is metadata. Require `missing_pipeline_stages_for_inventory` to
return an empty vector.

- [ ] **Step 2: Add failing dashboard integration test**

Seed a full-tagged `unknown/unknown` document, a `rejected` metadata document,
and a genuinely unknown untagged document. Require the first two to contribute
to `complete`, and only the last to contribute to `pending`.

- [ ] **Step 3: Run RED tests**

```bash
cargo test -p archivist-db missing_pipeline_stages_skip_rejected --lib
DATABASE_URL="$DISPOSABLE_DATABASE_URL" cargo test -p archivist-db --test dashboard_stage_status -- --ignored --test-threads=1
```

- [ ] **Step 4: Implement terminal/effective stage semantics**

Extend `stage_needs_work` to exclude `rejected`. In the stage-matrix CTE,
derive both OCR and metadata as `succeeded` when `has_full_completion_tag` is
true and count `rejected` with the resolved terminal statuses.

- [ ] **Step 5: Run GREEN tests and commit**

```bash
cargo test -p archivist-db --lib
DATABASE_URL="$DISPOSABLE_DATABASE_URL" cargo test -p archivist-db --test dashboard_stage_status -- --ignored --test-threads=1
git add crates/archivist-db/src/lib.rs crates/archivist-db/tests/dashboard_stage_status.rs
git commit -m "fix(dashboard): stop reporting resolved stages as pending"
```

### Task 3: Status-backed Paperless completion-tag reconciliation

**Files:**
- Modify: `crates/archivist-db/src/lib.rs`
- Modify: `crates/archivist-api/src/main.rs`
- Create: `crates/archivist-db/tests/completion_reconciliation.rs`

**Interfaces:**
- Produces: `completed_document_ids_missing_full_tag(&DbPool, &[Stage]) -> Result<Vec<i32>>`.
- Extends: `completion_tag_reconcile_needed(..., inventory_stages_complete: bool) -> bool`.

- [ ] **Step 1: Add failing database candidate tests**

Seed documents covering enabled-stage success, disabled-stage unknown, active
stage unknown, rejected terminal status, and an existing full tag. Require only
fully resolved documents without the global tag to be returned.

- [ ] **Step 2: Add failing API predicate tests**

Require a document with only `archivist-ocr` to reconcile when
`inventory_stages_complete=true`, remain unchanged when false, and remain
unchanged when `ai-processed` already exists.

- [ ] **Step 3: Run RED tests**

```bash
DATABASE_URL="$DISPOSABLE_DATABASE_URL" cargo test -p archivist-db --test completion_reconciliation -- --ignored --test-threads=1
cargo test -p archivist-api completion_tag_reconcile --bin archivist-api
```

- [ ] **Step 4: Implement query and endpoint integration**

Use static boolean binds for OCR/metadata enablement and terminal status arrays.
Build a `HashSet<i32>` before scanning Paperless documents, pass membership to
the predicate, and retain the endpoint's dry-run, filter, Paperless API, and
single audit-event behavior.

- [ ] **Step 5: Run GREEN tests and commit**

```bash
DATABASE_URL="$DISPOSABLE_DATABASE_URL" cargo test -p archivist-db --test completion_reconciliation -- --ignored --test-threads=1
cargo test -p archivist-api completion_tag_reconcile --bin archivist-api
git add crates/archivist-db/src/lib.rs crates/archivist-db/tests/completion_reconciliation.rs crates/archivist-api/src/main.rs
git commit -m "fix(paperless): reconcile completion tags from stage status"
```

### Task 4: Verification, release, deployment, and live repair

**Files:**
- Modify: release metadata and changelog files required by the repository release process.
- Verify: all files changed by Tasks 1-3.

**Interfaces:**
- Consumes: migration 0052 and the enhanced reconciliation endpoint.
- Produces: a released/deployed build and a consistent production inventory.

- [ ] **Step 1: Run repository verification**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
git diff --check origin/main...HEAD
```

- [ ] **Step 2: Review, merge, and publish the patch release**

Follow the repository's configured source and deployment release workflow. Require a green
pipeline before merging and a green tag pipeline before considering the release
published. Do not alter the `platform-ai/sglang-minimax` replica count.

- [ ] **Step 3: Verify migration and run reconciliation dry-run**

Confirm migration 0052 is installed, then call the authenticated endpoint for
document IDs 4579 and 4872 with `dry_run=true`. Require exactly two planned and
zero applied changes.

- [ ] **Step 4: Apply targeted tag reconciliation and sync**

Call the same endpoint with `dry_run=false`, require exactly two applied IDs,
then run/await a Paperless inventory sync. This path must emit the normal
`paperless.completion_tags_reconciled` audit event.

- [ ] **Step 5: Fresh live verification**

Require all of the following:

```text
active jobs = 0
OCR dashboard pending = 0
Metadata dashboard pending = 0
full-tag/status conflicts = 0
both-stages-succeeded-without-full-tag = 0
paperless_modified_at NULL rows = 0
```

Also confirm `platform-ai/sglang-minimax` remains at the operator-selected zero
replicas without changing it.
