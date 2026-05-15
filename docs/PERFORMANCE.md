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
