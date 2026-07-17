# Per-Provider Tuning Profiles — Contract

Working contract for milestone v1.6.2. Backend and frontend implementations
must agree on this shape. Same role as `METADATA_TRACE_CONTRACT.md`: locked
spec before parallel implementation. If something can't be implemented as
described, STOP and update this doc — don't silently improvise.

## Why

Today the tuning knobs that make a local-GPU deployment safe (low worker
concurrency, small context, OCR-page cap, throughput caps, no consensus)
vs the knobs that let a cloud-API deployment fly (high parallelism, full
context, no caps, consensus enabled) are scattered across `ai.*`,
`workflow.*`, `ocr.*`, `metadata.*` — and **`ARCHIVIST_WORKER_CONCURRENCY`
isn't in settings at all, only as an env var**. Switching between
"Ollama local on 4060 Ti" and "OpenAI cloud" today requires editing four
different sections plus a k8s redeploy. The v1.6.2 design lets both
configurations coexist in the same DB and switch by flipping
`ai.default_provider`.

## Data model

`AiProviderSettings` gains an optional `tuning: ProviderTuning` block.

```rust
pub struct AiProviderSettings {
    // ... existing fields ...
    #[serde(default)]
    pub tuning: ProviderTuning,
}

#[derive(Default)]
pub struct ProviderTuning {
    // --- Performance ---
    /// Worker pool size. Replaces ARCHIVIST_WORKER_CONCURRENCY as the live
    /// source of truth. The env stays as a hard upper cap (see §worker).
    #[serde(default)]
    pub worker_concurrency: Option<u32>,
    /// Secondary text model for two-model consensus check on correspondent
    /// + document_date. None = disabled (the v1.5.15 default).
    #[serde(default)]
    pub consensus_secondary_text_model: Option<String>,
    /// Day tolerance for the consensus check.
    #[serde(default)]
    pub consensus_date_tolerance_days: Option<i64>,
    /// Ollama text num_ctx override. Cloud providers ignore this.
    #[serde(default)]
    pub text_num_ctx: Option<i64>,
    /// Ollama vision num_ctx override. Cloud providers ignore this.
    #[serde(default)]
    pub vision_num_ctx: Option<i64>,

    // --- Resource caps ---
    /// OCR pages to extract per document. None = inherit global ocr.page_limit.
    #[serde(default)]
    pub ocr_page_limit: Option<u16>,
    /// Throughput safety caps. None = inherit workflow.*. None+None = uncapped.
    #[serde(default)]
    pub hourly_document_limit: Option<i64>,
    #[serde(default)]
    pub daily_document_limit: Option<i64>,

    // --- Quality thresholds (None = inherit MetadataSettings) ---
    #[serde(default)]
    pub metadata_confidence_threshold: Option<f32>,
    #[serde(default)]
    pub title_confidence_threshold: Option<f32>,
    #[serde(default)]
    pub correspondent_confidence_threshold: Option<f32>,
    #[serde(default)]
    pub document_type_confidence_threshold: Option<f32>,
    #[serde(default)]
    pub document_date_confidence_threshold: Option<f32>,
    #[serde(default)]
    pub tags_confidence_threshold: Option<f32>,
    #[serde(default)]
    pub fields_confidence_threshold: Option<f32>,
    #[serde(default)]
    pub max_tags: Option<u32>,
    #[serde(default)]
    pub allowed_list_max: Option<u32>,
}
```

Defaults shipped in code for the two presets — used as initial values for
new provider rows AND as "Reset to default" targets in the UI:

| Field | `Ollama` (local 4060 Ti class) | `OpenAI` (paid cloud) |
| --- | --- | --- |
| `worker_concurrency` | `Some(2)` | `Some(8)` |
| `consensus_secondary_text_model` | `None` | `Some("gpt-4o-mini")` |
| `consensus_date_tolerance_days` | `None` (→ 1) | `None` (→ 1) |
| `text_num_ctx` | `Some(4096)` | `None` |
| `vision_num_ctx` | `Some(4096)` | `None` |
| `ocr_page_limit` | `Some(2)` | `Some(8)` |
| `hourly_document_limit` | `Some(200)` | `None` |
| `daily_document_limit` | `Some(2000)` | `None` |
| `*_confidence_threshold` | all `None` (→ global default) | all `None` |
| `max_tags` | `None` | `None` |
| `allowed_list_max` | `None` | `None` |

These presets live as `AiProviderSettings::ollama_default()` /
`openai_default()` / etc. — the existing constructors. We just extend
them with the `tuning:` field.

### Measured SGLang/MiniMax M3 preset

The exact built-in `sglang-minimax-m3` identity has a dedicated preset rather
than inheriting the generic OpenAI-compatible values:

| Field | Value |
| --- | --- |
| `worker_concurrency` | `Some(1)` |
| `reasoning_effort` | `None` |
| `max_output_tokens` | `Some(4096)` |
| `structured_output` | `Some(Auto)` |
| `request_timeout_seconds` | `Some(180)` |
| all other tuning fields | `None` |

Core construction, frontend suggestion, and Reset-to-defaults all share these
values. The generic `openai_compatible` preset remains unchanged. The measured
rationale and load results are in
[`docs/performance/2026-07-17-sglang-minimax-m3-capacity.md`](performance/2026-07-17-sglang-minimax-m3-capacity.md).

## Resolution rule

A new helper on `RuntimeSettings`:

```rust
impl RuntimeSettings {
    /// Returns the effective tuning for the currently-active default provider,
    /// layered over the global settings fields the tuning can override.
    pub fn effective_tuning(&self) -> EffectiveTuning;
}

pub struct EffectiveTuning {
    pub worker_concurrency: u32,
    pub consensus_secondary_text_model: Option<String>,
    pub consensus_date_tolerance_days: i64,
    pub text_num_ctx: Option<i64>,
    pub vision_num_ctx: Option<i64>,
    pub ocr_page_limit: u16,
    pub hourly_document_limit: Option<i64>,
    pub daily_document_limit: Option<i64>,
    pub metadata_confidence_threshold: f32,
    pub title_confidence_threshold: f32,
    pub correspondent_confidence_threshold: f32,
    pub document_type_confidence_threshold: f32,
    pub document_date_confidence_threshold: f32,
    pub tags_confidence_threshold: f32,
    pub fields_confidence_threshold: f32,
    pub max_tags: u32,
    pub allowed_list_max: u32,
}
```

Rules:

1. The active provider is `ai.providers.find(|p| p.name == ai.default_provider)`.
   If not found, fall through to the first `enabled` provider.
2. For each field in `EffectiveTuning`:
   - If `active.tuning.<field>` is `Some(v)` → use `v`.
   - Else fall back to the existing global location (`ai.*`,
     `workflow.*`, `ocr.*`, `tagging.*`, `metadata.*`, `fields.*`).
3. Per-stage provider overrides (`ai.stage_models[]`) **do not** change
   the resolution — they only change which model is called. Mental
   model: "I'm on provider X, X's limits apply to the whole workflow."
   The one exception is `ocr_page_limit`: this is OCR-stage-specific, so
   it resolves against the OCR-stage provider's tuning. Worker code
   already has the per-stage provider context at the OCR call site, so
   this is a localised lookup.

Every existing call site that reads `settings.ai.consensus_secondary_text_model`,
`settings.ai.ollama_text_num_ctx`, `settings.ocr.page_limit`,
`settings.workflow.hourly_document_limit`, etc. needs to route through
`settings.effective_tuning()` instead. Backend issue lists the call sites
to migrate.

## Worker live-reload of concurrency

Today the worker pool size is fixed at startup from
`ARCHIVIST_WORKER_CONCURRENCY` env. Change:

* The env stays as a **hard upper cap** — settings value is clamped to
  `min(env_cap, settings_value)`. Stops an operator-typo from spinning
  up 1000 concurrent jobs.
* On every `claim_jobs` cycle (already runs every few seconds), the
  worker reads the live settings and computes the new target
  concurrency. If different from current pool size:
  - Increase → spawn additional workers up to the target.
  - Decrease → mark surplus workers to exit after their current job
    completes. Never abort an in-flight job.
* Hot-reload metrics: emit an audit event `workflow.concurrency_changed`
  with `from`/`to` whenever the live value differs from the previous
  cycle's value.

Implementation hint: the worker's task supervisor already manages
N spawned tasks. Add a `target_concurrency: AtomicU32` channel; the
spawn loop checks it before claiming the next job.

## Ollama runtime hints endpoint

`GET /api/ai/runtime-hints` returns whatever Ollama reports about its
own state. Requires `read_settings`.

```jsonc
{
  "provider": "ollama",
  "reachable": true,
  "version": "0.5.7",
  "loaded_models": [
    { "name": "qwen3-paperless:8b", "size_vram_bytes": 6396411904 }
  ],
  // These are env-only on the Ollama pod and not exposed by its API.
  // Field set to null with a hint message — frontend shows the warn box.
  "num_parallel": null,
  "max_loaded_models": null,
  "hint": "NUM_PARALLEL, MAX_LOADED_MODELS, KEEP_ALIVE are set on the Ollama deployment, not in Archivist. Edit the Ollama k8s manifest to change them."
}
```

The handler hits Ollama's `/api/version` and `/api/ps`. If the provider
isn't Ollama, the endpoint returns `{ "provider": <name>, "reachable":
true_or_false, "hint": "<provider>-specific tuning is not server-side
observable from Archivist." }`. Cloud providers (OpenAI, Anthropic) don't
have a meaningful "loaded model" or "VRAM" concept — surface a stub.

## UI structure

Inside `Settings → AI`, the existing provider list gets a per-card
**"Tuning"** disclosure section beneath the connection fields. Three
sub-blocks, collapsible:

1. **Performance** — worker_concurrency, consensus_secondary_text_model
   (free-text + helper text), consensus_date_tolerance_days,
   text_num_ctx, vision_num_ctx.
2. **Resource caps** — ocr_page_limit, hourly_document_limit,
   daily_document_limit. Each labelled with the global default it
   overrides ("Default: 3 pages").
3. **Quality thresholds** — the 7 confidence thresholds plus max_tags
   and allowed_list_max, all as number inputs with min/max bounds.

A "Reset to defaults" button per sub-block writes the constructor's
preset values for that provider.

For the active provider (Ollama by default), a fourth read-only block
**"Ollama server hints"** below the Tuning section calls the new
runtime-hints endpoint and displays:

- Loaded model name + VRAM used
- Ollama version
- An inline warning box for `NUM_PARALLEL` / `MAX_LOADED_MODELS` /
  `KEEP_ALIVE` explaining they're deploy-time only with a copy-pasteable
  `kubectl set env deploy/ollama -n <ns> OLLAMA_NUM_PARALLEL=2` example.

For non-Ollama providers, the fourth block is hidden.

i18n keys: `settings.tuning.*` (performance / caps / thresholds / hints
nested), `settings.tuning.field.<name>`, `settings.tuning.reset_defaults`,
`settings.tuning.hint.ollama_env`. Add across all 7 locales.

## Tests

1. **Unit (archivist-core)**: table-driven `effective_tuning` tests
   covering: tuning value present (uses tuning), tuning value None
   (uses global), no active provider found (falls back to first
   enabled), per-stage override doesn't bleed into other stages.
2. **Unit (archivist-core)**: each new `*_default()` constructor
   deserializes from a fresh `Default::default()` cleanly and produces
   the expected preset values.
3. **Serde regression**: an existing `AiProviderSettings` JSON blob
   *without* the `tuning` field deserializes with `ProviderTuning::default()`
   populated. Same pattern as the v1.5.20 `completion_metadata` test.
4. **Worker live-reload**: spawn a worker with concurrency=2 in a test
   harness, mutate the settings to 4, assert the pool grows on the
   next claim cycle. Mutate to 1, assert the pool shrinks without
   aborting an in-flight job.
5. **Runtime-hints endpoint**: unit-test the response shape for Ollama
   (with a mock `/api/ps` response) and for a non-Ollama provider
   (returns the stub shape).
6. **migration_smoke**: nothing new — no schema change. The serde
   regression test in #3 covers the upgrade path.
7. **Frontend**: tsc + i18n parity + build. Targeted vitest for the
   `effectiveTuning` helper if one ends up in the frontend.

## Risk inventory

* **Per-provider quality thresholds** mean every `metadata.*_threshold`
  call site in `archivist-worker` must be routed through
  `effective_tuning()`. Many call sites — risk of missing one and
  silently using stale globals. **Mitigation**: write the resolution
  helper first; add a clippy-style search-and-replace step listed in
  the backend issue.
* **Worker pool downscale** must not abort in-flight jobs. Use a
  "shrink token" pattern: surplus tasks set a flag to exit-after-job.
  Add a test for the in-flight invariant.
* **OCR page_limit per-stage-provider** is the one resolution
  exception. Backend code needs to thread the stage provider through
  to the OCR helper. Localised change.
* **Settings save race**: an operator editing tuning while the worker
  reads it → standard read-after-write semantics, no special handling
  needed. The next claim cycle picks it up.
* **Settings schema breaking change for cloud users**: if anyone has a
  custom provider name not in `default_providers()`, they get
  `ProviderTuning::default()` (all `None`) → behaviour identical to
  today. Safe.

## Out of scope for v1.6.2

* Auto-tuning (system measures throughput and picks values).
* A/B comparison between two provider tunings.
* Per-document-type tuning.
* Cost-budget enforcement based on `cost_per_1m_*` × actual tokens.
