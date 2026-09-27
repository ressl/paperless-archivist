# Performance And Sizing

Status: v1.0 GA readiness

Paperless Archivist is designed for real Paperless-ngx archives. The main
scaling dimensions are document inventory size, queued jobs, model latency, and
Paperless API latency.

## Benchmark Scope

The GA benchmark focuses on:

- 10,000 document inventory
- 50,000 document inventory
- optional 100,000 document inventory
- dashboard backlog counts
- inventory pagination
- job activity queries
- worker claim query
- auto-selector candidate scan

The benchmark does not call Paperless or model providers. It isolates the
PostgreSQL query paths that determine UI responsiveness and worker queue
throughput.

## Running The Benchmark

Docker is required. The script starts an isolated PostgreSQL 18 container,
applies all migrations, generates synthetic inventory/jobs, and writes a report
under `target/perf`.

```bash
scripts/perf/run_postgres_inventory_benchmark.sh
```

Use smaller or larger datasets:

```bash
BENCH_SIZES="10000 50000" scripts/perf/run_postgres_inventory_benchmark.sh
BENCH_SIZES="100000" scripts/perf/run_postgres_inventory_benchmark.sh
```

The report includes `EXPLAIN (ANALYZE, BUFFERS)` for each query. Use it when
changing dashboard, inventory, queue, or selector SQL.

## GA Indexes

The v1.0 GA migration adds indexes for large-archive paths:

- `document_inventory_current_run_idx`
- `document_inventory_incomplete_idx`
- `document_inventory_current_tags_gin_idx`
- `jobs_created_at_idx`
- `jobs_status_updated_at_idx`
- `ai_artifacts_created_at_idx`
- `pipeline_runs_trigger_created_idx`

These complement the primary keys and existing status, lease, review, audit,
chat, and modified-timestamp indexes.

## Practical Sizing

| Archive size | API replicas | Worker replicas | Worker concurrency | PostgreSQL guidance |
| --- | ---: | ---: | ---: | --- |
| Up to 10k documents | 1 | 1 | 1-2 | 2 vCPU, 2-4 GB RAM |
| 10k-50k documents | 1-2 | 1-2 | 2-4 | 4 vCPU, 4-8 GB RAM |
| 50k-100k documents | 2 | 2+ | 4+ | 4-8 vCPU, 8+ GB RAM |

Model inference is usually the bottleneck. For local Ollama, size GPU/VRAM for
the selected text and vision models before increasing worker concurrency.

### SGLang/MiniMax M3 measured profile

The built-in `sglang-minimax-m3` provider uses a measured conservative profile:

- worker concurrency `1` (still clamped by the global
  `ARCHIVIST_WORKER_CONCURRENCY` hard upper cap)
- request timeout `180` seconds
- maximum output `4096` tokens
- structured output `auto` (strict JSON Schema where the consumer supplies it)
- reasoning unset/disabled by default

This reserves one of the reviewed runtime's two request slots for interactive
Prompt Tester, Provider Test, or Document Chat traffic. With structured output
`auto`, one high-level call may make a schema request and one bounded
compatibility retry. The Worker lease is therefore at least 420 seconds
(`2 × 180 + 60`), covering both per-request timeouts plus its safety margin.
Do not raise Worker concurrency merely because a short parallel smoke
succeeds: offering four Worker requests to the two-slot runtime reduced Worker
throughput by about 13% versus two while increasing p50 latency from 1.01 to
2.82 seconds. See the public-safe
[capacity report](performance/2026-07-17-sglang-minimax-m3-capacity.md) for
method, p50/p95, throughput, timeout/error rates, the mixed application-path
E2E gate, retry bounds, and the revalidation command.

This profile is evidence for the exact reviewed pins and two request slots,
not a general SGLang capacity promise. Follow the
[M3 operations runbook](OPERATIONS.md#sglangminimax-m3-operations) for the
validation order and symptom-specific timeout, parser, schema, authentication,
and NetworkPolicy diagnostics before changing the profile.

### OCR vision fallback lease fencing

One OCR page can involve three high-level provider calls when an Ollama vision
runtime crashes: the primary vision request, local model discovery through
`/api/tags`, and one fallback vision request. The Worker renews its
owner-scoped database lease immediately before each call. It does not rely on
one lease window to cover the whole chain. Model discovery uses the same
resolved `request_timeout_seconds` as the primary and fallback clients, so each
step is bounded by the timeout used to size the lease.

If any renewal reports that another Worker owns the job, the protected network
future is never polled. OCR exits without writing a page cache entry, fallback
success audit, job completion, review, or Paperless apply result. Search Worker
logs for `OCR lease lost` and the structured `vision_phase` value (`primary`,
`model_discovery`, or `fallback`) to locate the boundary. Persistent lease loss
usually means Worker replicas or database latency exceed the configured model
capacity; inspect expired leases and queue age before raising concurrency.

### Paperless inventory sync (#408)

The Worker syncs the Paperless document list every minute (full list unless
`paperless.delta_sync_enabled`). Since 2026-09 the sync:

- requests `/api/documents/?fields=id,title,created,modified,tags,correspondent,document_type,original_file_name&truncate_content=true`,
  so no OCR `content` is transferred or deserialized (the same helper serves
  the API sync and the consistency check);
- commits the tag/correspondent/type/custom-field catalog first, then upserts
  inventory rows in id-ordered transactions of 500 rows, and only then advances
  the sync cursor (a crash mid-sync re-covers the window next time);
- locks inventory rows in ascending id order in `claim_jobs` as well, so the
  claim path and the sync cannot deadlock.

The operator-triggered sync in the API still upserts in a single transaction
(it now also skips `content`); batching it the same way is a follow-up.

Expected impact (estimated, not yet measured on a production archive): the
list payload shrinks from roughly `documents × average OCR text` (about
10-50 KB per document, i.e. hundreds of MB per minute at 20k documents) to
about 0.3 KB per document (~6 MB at 20k); peak Worker memory during the sync
drops by the same order; the longest sync transaction holds at most 500 row
locks instead of the whole archive, so `claim_jobs`, `complete_job` and
`fail_job` wait at most one batch. Record measured before/after numbers here
with `scripts/perf/run_postgres_inventory_benchmark.sh` and Worker RSS when a
large archive is available. The `delta_sync_enabled` default stays `false`:
the full list is now cheap, and flipping the default would change the
runtime-settings contract.

### Claim query plan (#412)

`claim_jobs` claims in up to three passes, each served by an index in
`ORDER BY` order and stopped after `limit` rows: expired leases via
`jobs_lease_until_idx`, queued retries via the partial `idx_jobs_claim_retry`
(migration 0053), and remaining queued jobs via `idx_jobs_claim`. The ignored
DB test `claim_job_passes_use_ordered_indexes`
(`crates/archivist-db/tests/claim_jobs_stale_lease.rs`) asserts via `EXPLAIN`
that each pass scans its index and plans no `Sort` node.

## Operational Targets

Use these as practical targets, not strict promises:

- dashboard initial load under a few seconds on 50k documents
- inventory page query under a few hundred milliseconds on indexed PostgreSQL
- worker claim query under a few hundred milliseconds with a normal queue
- sync throughput limited mostly by Paperless REST API response time
- OCR/tagging throughput limited mostly by model latency

If the database is slow, inspect:

- missing PostgreSQL autovacuum/analyze
- excessive job history without retention
- very deep inventory offsets
- slow storage for PostgreSQL
- too many concurrent workers for the model provider

## GA Benchmark Snapshot

The v1.0 GA benchmark was run with:

```bash
BENCH_SIZES="10000 50000" scripts/perf/run_postgres_inventory_benchmark.sh
```

Report path:

```text
target/perf/postgres-inventory-benchmark.txt
```

Observed PostgreSQL 18 query timings on the local Docker benchmark:

| Dataset | Backlog counts | First inventory page | Deep inventory page | Job activity | Worker claim | Auto-selector scan |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10k docs | 1.7 ms | 0.1 ms | 1.0 ms | 4.9 ms | 33.9 ms | 0.4 ms |
| 50k docs | 8.4 ms | 0.2 ms | 5.0 ms | 4.7 ms | 34.4 ms | 0.3 ms |

The worker claim benchmark used 70k synthetic jobs. Model calls and Paperless API
latency are not included in these timings.

## Retention

Long-running systems should configure artifact retention and audit retention
according to local policy. Keeping raw AI artifacts forever is rarely needed and
can increase database size quickly.
