# PostgreSQL 18 Design

Status: draft  
Decision: PostgreSQL 18 is mandatory  
Purpose: use PostgreSQL 18 as the durable workflow, audit, reporting, and AI
state engine for Paperless Archivist.

## 1. Why PostgreSQL 18 Is Required

Paperless Archivist is not only a stateless companion service. It needs a
reliable state engine for long-running document processing:

- queueing
- retries
- per-document stage status
- prompt versioning
- AI artifacts
- review workflows
- audit logs
- backlog reporting
- batch processing of all existing documents
- future semantic search/RAG

PostgreSQL 18 is a good fit because it improves both operational performance and
developer ergonomics for this exact workload:

- better large-table scans and vacuum through asynchronous I/O
- timestamp-ordered UUIDs with `uuidv7()`
- generated columns for query-friendly derived state
- better multicolumn-index usage through skip scans
- `OLD`/`NEW` in `RETURNING` for clean audit trails
- temporal constraints for non-overlapping schedules and leases
- better observability through richer `EXPLAIN` and table statistics
- page checksums enabled by default for data safety

## 2. PostgreSQL 18 Features We Will Use

### 2.1 `uuidv7()` for Primary Keys

Use `uuidv7()` as the default ID generator for Archivist-owned tables.

Why:

- UUIDs remain globally unique.
- IDs are roughly time ordered.
- B-tree indexes have better locality than random UUIDv4.
- Runs, jobs, artifacts, and audit events are naturally sortable by ID and time.

Use for:

- `pipeline_runs.id`
- `jobs.id`
- `ai_artifacts.id`
- `review_items.id`
- `audit_events.id`
- `prompts.id`

Example:

```sql
id uuid primary key default uuidv7()
```

### 2.2 Asynchronous I/O

PostgreSQL 18 introduces asynchronous I/O for operations such as sequential
scans, bitmap heap scans, and vacuum.

Why it matters for Archivist:

- backlog reports scan many rows
- audit history grows continuously
- AI artifacts can become large
- periodic cleanup/vacuum must stay cheap
- all-document status dashboards must remain fast

Operational recommendation on Linux:

```text
io_method=io_uring
```

Fallback if the platform cannot use `io_uring`:

```text
io_method=worker
```

### 2.3 Multicolumn B-tree Indexes and Skip Scan

PostgreSQL 18 can use multicolumn B-tree indexes in more query shapes through
skip scan.

Archivist should still design indexes intentionally, but skip scan makes common
dashboard and worker queries more forgiving.

Useful indexes:

```sql
create index jobs_status_run_after_idx
  on jobs (status, run_after, stage);

create index jobs_document_stage_status_idx
  on jobs (paperless_document_id, stage, status);

create index pipeline_runs_document_status_idx
  on pipeline_runs (paperless_document_id, status, created_at desc);

create index audit_events_document_created_idx
  on audit_events (paperless_document_id, created_at desc);
```

Query examples that must be fast:

- all pending jobs ordered by `run_after`
- all failed OCR jobs
- all documents missing tagging
- all runs for one Paperless document
- all audit events for one document

### 2.4 Virtual Generated Columns

PostgreSQL 18 makes virtual generated columns the default. They compute on read
instead of storing duplicate data.

Use them to expose frequently queried values from JSONB without duplicating
application logic.

Good candidates:

- `jobs.stage_key` derived from payload
- `jobs.priority` derived from payload
- `pipeline_runs.stage_count` derived from `stages`
- `ai_artifacts.model_name` derived from response/request JSON when not stored
  explicitly
- status booleans in inventory/cache tables

Example:

```sql
alter table jobs
  add column priority int
  generated always as ((payload ->> 'priority')::int) virtual;
```

Rule:

Use real columns for core workflow state. Use generated columns for convenience
and reporting over JSONB payloads.

### 2.5 JSONB for AI Artifacts and Flexible Stage Payloads

PostgreSQL JSONB is not new in 18, but it is central to Archivist.

Use JSONB for:

- raw provider request
- raw provider response
- normalized AI output
- suggested Paperless patch
- edited review patch
- model-specific metadata
- stage configuration snapshot

Rules:

- store typed columns for workflow state (`status`, `stage`, `run_after`)
- store flexible AI data in JSONB
- validate JSONB shape in application code
- add JSONB GIN indexes only where query patterns require them

Useful indexes:

```sql
create index ai_artifacts_normalized_output_gin_idx
  on ai_artifacts using gin (normalized_output jsonb_path_ops);

create index review_items_suggested_patch_gin_idx
  on review_items using gin (suggested_patch jsonb_path_ops);
```

PostgreSQL 18 also supports parallel GIN index builds, which helps when these
tables grow.

### 2.6 `OLD` and `NEW` in `RETURNING`

PostgreSQL 18 can return old and new values from `INSERT`, `UPDATE`, `DELETE`,
and `MERGE`.

Use this for audit-safe state transitions.

Example:

```sql
with changed as (
  update review_items
     set status = 'approved',
         reviewed_by = $1,
         reviewed_at = now()
   where id = $2
     and status = 'pending'
  returning old.status as old_status,
            new.status as new_status,
            new.id as review_id,
            new.run_id
)
insert into audit_events (run_id, event_type, actor, before, after)
select run_id,
       'review.approved',
       $1,
       jsonb_build_object('status', old_status),
       jsonb_build_object('status', new_status)
from changed;
```

This avoids race-prone "select before update, then update, then insert audit"
application logic.

### 2.7 `MERGE ... RETURNING`

Use `MERGE` for idempotent metadata sync from Paperless:

- tags
- correspondents
- document types
- custom fields
- document inventory rows

Use `RETURNING` to record changed values for audit or sync statistics.

Example use case:

- Paperless tag renamed from `rechnung` to `Rechnung`
- metadata sync updates cache
- sync summary records that one tag changed

### 2.8 Temporal Constraints

PostgreSQL 18 adds temporal constraints over ranges using `WITHOUT OVERLAPS` and
`PERIOD`.

Use this where non-overlap matters:

- active document processing locks
- model configuration validity windows
- prompt activation history
- maintenance windows

Example concept:

```sql
valid_during tstzrange not null,
unique (stage, valid_during without overlaps)
```

For MVP, normal row locks are enough for job leasing. Temporal constraints are
most useful for durable scheduling and history where overlapping active config
must be impossible.

### 2.9 `FOR UPDATE SKIP LOCKED` Job Leasing

This is not new in PostgreSQL 18, but it is mandatory for the worker design.

Use it to let multiple workers safely claim jobs:

```sql
with claimed as (
  select id
    from jobs
   where status = 'queued'
     and run_after <= now()
   order by run_after, created_at
   for update skip locked
   limit $1
)
update jobs
   set status = 'running',
       lease_owner = $2,
       lease_until = now() + interval '5 minutes',
       attempts = attempts + 1
 where id in (select id from claimed)
returning *;
```

### 2.10 `LISTEN` / `NOTIFY`

This is not new in PostgreSQL 18, but it avoids worker sleep delays.

Use it when API creates jobs:

- API inserts job
- DB trigger or API emits `NOTIFY archivist_jobs`
- workers wake immediately
- polling remains as fallback

### 2.11 Partitioning

Use partitioning for append-heavy tables:

- `audit_events`
- `ai_artifacts`
- optionally `jobs` after completion

Recommended partition key:

```text
created_at monthly
```

Why:

- fast retention cleanup
- smaller indexes
- faster vacuum
- easier archival/export

### 2.12 Materialized Views for Backlog Reports

Use a materialized view or incrementally maintained inventory table for the main
"what is still open?" dashboard.

The dashboard must answer:

- total documents known from Paperless
- documents missing OCR
- documents missing tagging
- documents in review
- documents failed by stage
- documents currently queued/running
- documents fully processed
- documents never seen by Archivist

Recommended MVP:

- maintain `document_inventory` table during Paperless sync
- refresh state after every job completion
- optionally expose a materialized aggregate view for dashboard counts

### 2.13 Full Text Search, `pg_trgm`, and `casefold()`

Use PostgreSQL text tools for local matching and diagnostics:

- `tsvector` for searching run history, errors, prompt names
- `pg_trgm` for fuzzy matching of tag/correspondent names
- `casefold()` for better case-insensitive comparisons
- `PG_UNICODE_FAST` collation where appropriate for fast Unicode behavior

Important:

AI decisions must not rely only on fuzzy DB matching. Fuzzy matching is a helper
for review, diagnostics, and candidate selection.

### 2.14 `pgvector` for Future RAG

`pgvector` is not a PostgreSQL core feature, but it should be the preferred
extension for future semantic search if available on PostgreSQL 18.

Use later for:

- document embeddings
- chunk embeddings
- similar documents
- semantic search
- RAG chat

Do not make `pgvector` required for the MVP. Design the schema so it can be added
without changing the job pipeline.

### 2.15 `COPY FROM ... ON_ERROR ignore REJECT_LIMIT`

Use for bulk imports and migration tooling:

- importing fixture datasets
- importing exported classification results
- loading document inventory snapshots

Set a reject limit so bad rows do not silently hide broken imports.

### 2.16 Page Checksums by Default

PostgreSQL 18 enables page checksums by default for new clusters.

Use this as an operational requirement:

- keep checksums enabled
- monitor checksum failures
- document backup/restore procedure

### 2.17 Observability Improvements

Use PostgreSQL 18 observability features during performance work:

- `EXPLAIN ANALYZE` includes buffer details automatically
- more index lookup details in `EXPLAIN ANALYZE`
- more vacuum timing in `pg_stat_all_tables`
- per-connection I/O and WAL utilization statistics

This matters for:

- slow backlog reports
- inefficient dashboard queries
- growing audit/artifact tables
- worker lease contention

### 2.18 Authentication and Security

PostgreSQL 18 deprecates MD5 password authentication.

Requirements:

- use SCRAM for password authentication
- never use MD5 passwords
- store credentials in Kubernetes secrets
- consider OAuth database authentication later if the platform standardizes on it

## 3. Database Responsibilities

PostgreSQL is responsible for:

- durable job queue
- idempotent pipeline runs
- AI artifacts
- review queue
- audit events
- document inventory
- backlog reporting
- prompt history
- model configuration
- optional future embeddings

PostgreSQL is not responsible for:

- storing original document files
- replacing Paperless-ngx metadata as source of truth
- keeping Paperless user passwords
- doing AI inference

## 4. Document Inventory Model

Archivist must be able to process all documents and show what is still open.

Add a `document_inventory` table that mirrors minimal Paperless state:

```sql
create table document_inventory (
  paperless_document_id integer primary key,
  title text,
  original_file_name text,
  current_tags text[] not null default '{}',
  has_ocr_completion_tag boolean not null default false,
  has_tagging_completion_tag boolean not null default false,
  has_full_completion_tag boolean not null default false,
  ocr_status text not null default 'unknown',
  tagging_status text not null default 'unknown',
  title_status text not null default 'unknown',
  correspondent_status text not null default 'unknown',
  document_type_status text not null default 'unknown',
  fields_status text not null default 'unknown',
  last_seen_at timestamptz not null default now(),
  last_run_id uuid,
  next_required_stage text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);
```

The inventory is refreshed by:

- scheduled Paperless sync
- manual sync endpoint
- job completion
- review apply

## 5. Backlog Views

Expose backlog views for reporting and API queries.

Example:

```sql
create view document_backlog as
select
  paperless_document_id,
  title,
  ocr_status,
  tagging_status,
  title_status,
  correspondent_status,
  document_type_status,
  fields_status,
  case
    when ocr_status not in ('succeeded', 'skipped') then 'ocr'
    when tagging_status not in ('succeeded', 'skipped') then 'tags'
    when title_status not in ('succeeded', 'skipped') then 'title'
    when correspondent_status not in ('succeeded', 'skipped') then 'correspondent'
    when document_type_status not in ('succeeded', 'skipped') then 'document_type'
    when fields_status not in ('succeeded', 'skipped') then 'fields'
    else null
  end as next_stage
from document_inventory;
```

Dashboard aggregates:

```sql
select next_stage, count(*)
from document_backlog
group by next_stage
order by next_stage;
```

## 6. Completion Tag Behavior

Workflow tags are managed by code, never by prompts.

Required behavior:

- create missing workflow tags automatically on startup or first use
- add stage completion tags only after successful stage completion
- remove trigger tags after successful stage completion
- keep failure tags optional and configurable
- never offer workflow tags as normal tag candidates to AI

Recommended workflow tags:

```text
ai-process
ai-ocr
ai-tags
ai-title
ai-correspondent
ai-document-type
ai-fields
ai-processed
ai-processed-ocr
ai-processed-tagging
ai-processed-title
ai-processed-correspondent
ai-processed-document-type
ai-processed-fields
ai-review-needed
ai-failed
ai-failed-ocr
ai-failed-tagging
```

## 7. MVP Database Extensions

Required:

```sql
create extension if not exists pg_trgm;
```

Optional later:

```sql
create extension if not exists vector;
```

`pgcrypto` is optional. Prefer application-side hashing unless a DB-side digest
is useful for import tooling or constraints.

## 8. Operational Requirements

- PostgreSQL major version must be 18.
- Use a dedicated database and role.
- Use SCRAM authentication.
- Keep page checksums enabled for new clusters.
- Back up database before destructive batch operations.
- Retain `audit_events` longer than `ai_artifacts`.
- Consider monthly partition retention for large installations.
- Run migrations through SQLx.
- CI must test against PostgreSQL 18, not older versions.

## 9. Features Not Worth Depending On in MVP

Some PostgreSQL 18 features are useful operationally but should not shape the MVP
application model:

- OAuth database authentication: useful later, not needed for app runtime.
- Logical replication improvements: useful for HA/backup topology, not MVP logic.
- Foreign table `LIKE`: useful only if we later sync external sources.
- Temporal foreign keys: powerful, but not needed before schedules/config history
  become complex.

The MVP should use PostgreSQL 18 strongly, but avoid clever database design that
slows down the first implementation.
