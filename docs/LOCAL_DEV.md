# Local end-to-end dev stack

A self-contained docker compose overlay that brings up Postgres, the archivist
API/worker, Paperless-ngx + Redis, all wired to use **host-installed Ollama**.
It is built for one workflow: pull a small varied sample of real documents
from production Paperless-ngx (read-only) and drive the entire v1.4.x
pipeline against them locally before merging.

## Topology

```
host Ollama  <---- http://host.docker.internal:11434
        ^
        |
+-------+----------------------------------------------+
| docker compose stack                                 |
|                                                      |
|  archivist-api  --+                                  |
|                   |                                  |
|  archivist-worker +--> postgres (5432, exposed 55433)|
|                   |                                  |
|                   +--> paperless-ngx (8000)          |
|                                                      |
|                   paperless-redis (6379)             |
+------------------------------------------------------+
```

Service / host ports:

| Service       | Internal              | Host (loopback) |
|---------------|-----------------------|-----------------|
| archivist API | `api:8080`            | `127.0.0.1:18080` |
| Paperless-ngx | `paperless-ngx:8000`  | `127.0.0.1:8000`  |
| Postgres      | `postgres:5432`       | `127.0.0.1:55433` |
| Ollama (host) | `host.docker.internal:11434` | `127.0.0.1:11434` |

## Quickstart

```bash
# 1. Copy the env template and edit if needed
cp deploy/compose/.env.dev-local.example deploy/compose/.env.dev-local
$EDITOR deploy/compose/.env.dev-local

# 2. Build + start the stack
docker compose \
  --env-file deploy/compose/.env.dev-local \
  -f deploy/compose/docker-compose.yml \
  -f deploy/compose/dev-local-docker-compose.yml \
  up --build -d

# 3. Bootstrap: wait for Paperless, mint its API token, configure archivist
./scripts/dev-local-bootstrap.sh

# 4. Seed the local Paperless from production (READ-ONLY against prod)
./scripts/dev-local-seed.sh --count 20

# 5. Drive the full end-to-end pipeline + approve everything
./scripts/dev-local-e2e.sh
```

The bootstrap script writes its captured Paperless token / archivist CSRF /
session cookie under `scripts/.dev-local-state.json` and
`scripts/.dev-local-cookies.txt` (both gitignored). All follow-up scripts read
state from there.

## Ollama models

Pick a non-crashing vision model and a text model that exist in your local
Ollama. The bootstrap defaults are:

| Slot   | Default         | Notes                                                              |
|--------|-----------------|--------------------------------------------------------------------|
| Text   | `qwen3:30b`     | Used for the consolidated `Stage::Metadata` round-trip.            |
| Vision | `qwen2.5vl:7b`  | Used for OCR. ~5× faster than `qwen3-vl:32b` on a 64 GB Mac. **Do not** use `glm-ocr:*` — GGML_ASSERT crashes in prod. |

To switch models without re-running bootstrap from scratch, override the env
vars:

```bash
TEXT_MODEL=qwen3-coder:30b VISION_MODEL=qwen3-vl:32b \
  ./scripts/dev-local-bootstrap.sh
```

## Resetting the stack

```bash
docker compose \
  --env-file deploy/compose/.env.dev-local \
  -f deploy/compose/docker-compose.yml \
  -f deploy/compose/dev-local-docker-compose.yml \
  down -v

rm -f scripts/.dev-local-state.json scripts/.dev-local-cookies.txt
rm -rf dev-samples
```

`down -v` also drops the Postgres + Paperless volumes, so the next `up` runs
the init scripts again (creating the `paperless` database in addition to the
archivist one). If you skip `-v` you keep the previous documents in Paperless
but a fresh archivist DB cannot be migrated cleanly — always pair the two.

## Pulling samples from an upstream Paperless

`scripts/seed-from-prod.py` walks an upstream Paperless inventory (read-only),
picks a varied set of N documents (default 20) across correspondents, document
types, dates, and file sizes, downloads them to `dev-samples/` (gitignored),
and re-uploads each one to the local Paperless. The original `title`,
`created` date, correspondent, document type, and tags are carried over so the
archivist sees realistic metadata.

The upstream URL and API token are read from the shell environment only —
nothing source-sensitive is ever written to disk:

```bash
export SOURCE_PAPERLESS_URL=https://paperless.example.com
export SOURCE_PAPERLESS_TOKEN=xxxxxxxxxxxxxxxxx
./scripts/dev-local-seed.sh --count 20
```

Mint the token in your upstream Paperless UI under *Profile → API Auth Token*.
Use a read-only account if your deployment supports it; the script never writes
back to the source.

## Known prod bugs reproduced by this stack

Running `./scripts/dev-local-verify.sh` after a full e2e drive surfaces the
prod bugs we're tracking. These are **NOT** dev-local-stack issues — they are
upstream Rust bugs that the prod-fix agent owns. The local stack is the
reproduction harness:

1. **`/api/dashboard.stats.stage_status` returns the 7 legacy per-field rows
   instead of `["ocr","metadata"]`** — confirmed in
   `archivist-db/src/lib.rs::stage_status` (~ line 3083). The SQL builds the
   stage list from `union all select 'title' …` etc. and never references the
   v1.4.x `metadata_status` column.
2. **`/api/batches/full` queues N single-stage runs per stage instead of one
   multi-stage run per doc** — confirmed in
   `archivist-api/src/main.rs::queue_full_batch` (~ line 3336). It loops over
   `enabled_stages` and calls `queue_missing_stage(..., stage, ...)` per stage
   rather than `queue_missing_pipeline(enabled_stages, ...)`. Symptom:
   `pipeline_runs.stages` is `["ocr"]` or `["metadata"]` for every row, never
   `["ocr","metadata"]`.
3. **Tag review_items store unresolved tag names (`Vec<String>`) where the
   patch applier expects `Vec<i32>`** — confirmed in
   `archivist-worker/src/main.rs::process_metadata` (~ line 1473-1481, and
   `1515-1517` for custom_fields). The validation-failure branch pushes
   `tags.tags.clone()` into the review_item as `"tags": [...]` (strings).
   Symptom: `/api/reviews/{id}/approve` returns
   `500 invalid type: string "Hardware", expected i32` and the autopilot
   drain warns "row returned to pending".

The `dev-local-verify.sh` script reports each of these as a separate `FAIL`
so progress on the fix is visible in CI/dev runs.

## Inspecting the running stack

```bash
# Open the archivist UI
open http://127.0.0.1:18080

# Open Paperless-ngx
open http://127.0.0.1:8000

# Tail worker logs
docker logs -f compose-worker-1

# psql into the archivist DB
psql postgres://archivist:dev-local-archivist-pg-password@127.0.0.1:55433/archivist

# Check ai_artifacts per stage for a given doc
psql postgres://archivist:dev-local-archivist-pg-password@127.0.0.1:55433/archivist \
  -c "select stage, count(*) from ai_artifacts where created_at > now() - interval '15 min' group by stage;"
```
