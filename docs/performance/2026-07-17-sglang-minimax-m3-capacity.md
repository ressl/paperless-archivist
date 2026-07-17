# SGLang/MiniMax M3 Capacity Report — 2026-07-17

Status: accepted for the built-in, disabled-by-default `sglang-minimax-m3`
provider preset.

## Target and method

The bounded harness `scripts/verify/sglang_minimax_m3_capacity.mjs` ran against
the reviewed deployment without changing its hardware or runtime
configuration. The target was pinned to:

- model: `ressl/MiniMax-M3-uncensored-NVFP4`
- model revision: `6863c5c62a892e2d1e886a69e134b3b866e0963e`
- SGLang revision: `0.0.0.dev1+g56e290315`
- image: `lmsysorg/sglang@sha256:8cc6e6f90bf803e9817800b679173d0b526f2b42b2c61b7ecafecdadb610eb55`
- reviewed runtime request slots: 2

The report intentionally omits endpoint, namespace, hostname, credentials,
hardware topology, prompts, and response bodies. Every request used generated
`SYNTHETIC-ONLY` content, disabled thinking, a 180-second request timeout, a
2 MiB response-body limit, and a 4096-token output limit. Worker Metadata used
a strict closed JSON schema. The mixed scenario alternated Worker Metadata and
Document Chat request shapes. Each run used one warm-up request.
This is a bounded smoke sample, not a statistically stable saturation
benchmark or a capacity guarantee for a differently configured deployment.

Verification has two layers. The table below is a live SGLang runtime
microbenchmark. The application-path gate
`worker_metadata_and_document_chat_m3_paths_share_endpoint_concurrently`
builds the Worker-equivalent Metadata prompt/schema and the real Document Chat
request through Archivist's Rust helpers, sends both concurrently through the
real `OpenAiCompatibleClient`, and asserts the M3 model, 4096-token cap,
disabled thinking, and schema/no-schema wire split. Worker unit tests separately
cover the global env clamp and the 420-second schema-fallback lease budget.
Together these prevent the runtime smoke from being mistaken for a full
deployed Archivist E2E test while still gating the mixed application paths.

## Results

| Run setting | Scenario | Actual concurrency | Requests | Throughput req/s | p50 ms | p95 ms | Timeout rate | Error rate |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | sequential Worker Metadata | 1 | 6 | 0.646 | 1,222 | 2,718 | 0% | 0% |
| 1 | parallel Worker Metadata | 1 | 6 | 0.953 | 901 | 1,374 | 0% | 0% |
| 1 | mixed Metadata + Chat | 1 | 6 | 1.808 | 208 | 931 | 0% | 0% |
| 2 | sequential Worker Metadata | 1 | 8 | 1.088 | 893 | 1,381 | 0% | 0% |
| 2 | parallel Worker Metadata | 2 | 8 | 1.463 | 1,010 | 2,429 | 0% | 0% |
| 2 | mixed Metadata + Chat | 2 | 8 | 1.988 | 331 | 2,328 | 0% | 0% |
| 4 | sequential Worker Metadata | 1 | 8 | 0.785 | 951 | 2,202 | 0% | 0% |
| 4 | parallel Worker Metadata | 4 | 8 | 1.275 | 2,818 | 3,965 | 0% | 0% |
| 4 | mixed Metadata + Chat | 4 | 8 | 2.450 | 1,285 | 1,948 | 0% | 0% |

All final hardened-harness scenarios completed without timeout or contract
error. In the actual parallel Worker Metadata phase, raising offered
concurrency from 2 to 4 reduced throughput from 1.463 to 1.275 req/s while p50
rose from 1,010 to 2,818 ms. Work above the runtime's two slots therefore
queues instead of increasing useful Worker capacity. An earlier exploratory
run observed a bounded 51-second `invalid_contract` result during an actual
concurrency-1 sequential phase. It is not attributed to concurrency 4 and is
not part of the final table, but is retained here as evidence that this small
smoke is not a latency guarantee. No response content was retained.

## Chosen profile

| Setting | Preset | Reason |
| --- | --- | --- |
| `worker_concurrency` | `1` | Leaves the second reviewed runtime slot available for Prompt Tester, Provider Test, or Document Chat instead of letting background work consume both slots. |
| `request_timeout_seconds` | `180` | Covers the final p95 values and the earlier 51-second exploratory outlier while remaining bounded. Because `auto` can make one schema request plus one compatibility fallback, the Worker lease is at least `2 × 180 + 60 = 420` seconds. |
| `max_output_tokens` | `4096` | Bounds cost and response growth while leaving sufficient room for metadata JSON. |
| `structured_output` | `auto` | Worker Metadata uses strict JSON Schema; schema-free consumers retain their normal response mode. |
| `reasoning_effort` | unset | ADR-014 keeps M3 thinking disabled unless a consumer explicitly requests a supported mode. |

No new process-local semaphore was added. It would not coordinate API and
worker replicas and could give a false global guarantee. The selected worker
profile, the deployment's bounded scheduler, and the global worker env cap
provide the pressure boundary. Operators diagnose pressure through
`paperless_archivist_metadata_latency_ms_p95`,
`paperless_archivist_oldest_queued_age_seconds`,
`paperless_archivist_job_retries_scheduled_total`, and
`paperless_archivist_model_errors_total`; worker pool changes remain auditable
as `workflow.concurrency_changed`.

Retries cannot amplify without bound: normal transient model failures consume
the job's finite `max_attempts` budget (default 3), and the special
infrastructure ceiling is 20 attempts with exponential backoff and jitter.
Provider quota failures enter cooldown instead of immediate retry.

## Revalidation

Run after changing the model revision, SGLang image/revision, request-slot
limit, context settings, or hardware. First update the shared exact pins in
`sglang_minimax_m3_contract.mjs`; the capacity harness rejects unreviewed pin
overrides. The endpoint variable must be a credential-free URL;
authentication, when needed, must use a readable file.

```bash
mkdir -p target
SGLANG_CAPACITY_BASE_URL=https://sglang.example.invalid/v1 \
SGLANG_CAPACITY_API_KEY_FILE=/run/secrets/sglang-key \
SGLANG_CAPACITY_REQUESTS=8 \
SGLANG_CAPACITY_CONCURRENCY=2 \
SGLANG_CAPACITY_REPORT_FILE=target/sglang-capacity.json \
node scripts/verify/sglang_minimax_m3_capacity.mjs
```

Exit `0` means every warm-up and measured request passed, `1` means a bounded
capacity/contract failure occurred, and `2` means configuration was rejected.
