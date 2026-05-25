# LLM Provider Settings: Model Sync, Reasoning Controls & Editable Catalog

**Status:** Planned (milestone `v1.6.3`)
**Author:** product planning, 2026-05-25
**Scope:** `archivist-core`, `archivist-ai`, `archivist-api`, `archivist-db`, `frontend`

## Motivation

The provider/model settings are largely static today:

- The recommended model lists live in a hard-coded `frontend/src/modelCatalog.ts`
  and the Rust `AiProviderSettings::default_providers()`. Keeping them current
  requires a code change and release.
- Only **Ollama** has live model discovery (`POST /model-providers/{name}/models`
  via `/api/tags`). OpenAI and Anthropic use the static catalog, and the Ollama
  Cloud provider (`kind: ollama`, `base_url: https://ollama.com`) is discovered
  through `/api/tags`, which does **not** reliably list the cloud catalog.
- There is **no reasoning-effort / thinking control**. The only request-shaping
  tuning today is the Ollama context window (`text_num_ctx` / `vision_num_ctx`).

The Ollama Cloud model matrix (see *Reference* below) shows that the strongest
defaults have shifted and that each model carries useful editorial metadata
(usage tier, context window, modality, best-for) that helps operators choose.
That metadata is **not** returned by any provider API — it is curated knowledge.

## Goals

1. A **"Sync model list"** button that pulls the live model IDs from **all three**
   provider families (Ollama Cloud, OpenAI, Anthropic) and keeps the available-ID
   list current without a release.
2. **Both** of the following for the "usage like high" request:
   - **Info badges**: show the curated matrix metadata (usage tier, context,
     modality, best-for) next to each recommended model.
   - **Reasoning effort**: a real, configurable request parameter
     (`off | low | medium | high`) sent to providers that support it.
3. Make the recommendation/tuning **catalog editable in Settings** (persisted in
   `runtime_settings`), rather than hard-coded.

## Design decisions (agreed)

- **Reasoning-effort scale:** `off | low | medium | high`. Default `medium`.
  Configured per provider (in `ProviderTuning`), optionally overridable per stage
  via `stage_models[]`.
- **Sync returns IDs only.** A live `/v1/models` (or `/api/tags`) call returns
  model identifiers plus minimal metadata. It never returns usage tier, context,
  or "best-for" — those stay editorial. Sync keeps *availability* honest; the
  editable catalog owns *recommendation + metadata*.
- **Catalog is the source of truth** for recommendation/metadata and is editable
  in Settings. Sync can append newly discovered IDs as un-annotated catalog
  entries for the operator to fill in.
- **Phased delivery**, one commit/issue per phase. Phases A and B deliver most of
  the practical value; Phase C (editable catalog) is the largest.

### ⚠️ Anthropic: thinking vs. constrained decoding

Constrained decoding for Anthropic (added in `f3699e5`) uses **forced tool-use**
(`tool_choice: {type: "tool", name: "emit_metadata"}`). Anthropic's extended
thinking is **not compatible** with a forced `tool_choice` — thinking requires
`tool_choice: auto`. Therefore, when reasoning effort `> off` is selected for an
Anthropic provider, the request must fall back to `tool_choice: auto`, which
weakens hard schema enforcement to best-effort. This trade-off is surfaced in the
UI. Ollama and OpenAI do not have this conflict.

---

## Phase A — Reasoning-effort tuning + Ollama Cloud defaults

**Crates:** `archivist-core`, `archivist-ai`; **frontend** types + a small UI control.

- `ChatRequest` (archivist-ai) gains `reasoning_effort: Option<ReasoningEffort>`
  where `ReasoningEffort` is `off | low | medium | high`.
- `ProviderTuning` + `EffectiveTuning` (+ TS `ProviderTuning`) gain
  `reasoning_effort`. Resolved like the other tuning knobs, with an optional
  per-stage override path through `stage_models[]`. Default `medium`.
- Apply in the request builders:
  - **OpenAI** (`build_openai_chat_payload`): set `reasoning_effort` for
    reasoning-capable models; omit otherwise.
  - **Ollama** (`build_ollama_chat_payload`): set `think: true/false` in
    `options` for thinking-capable cloud models (`off` → false, else true).
  - **Anthropic** (`build_anthropic_chat_payload`): set
    `thinking: {type: "enabled", budget_tokens}` mapped from the level
    (e.g. low ≈ 1024, medium ≈ 4096, high ≈ 16000). When enabled, drop the
    forced `tool_choice` (see caveat above).
- Refresh the Ollama Cloud **default models** to the matrix (`glm-5.1` text,
  `qwen3-vl:235b` / `qwen3-vl:235b-instruct` vision) and set a sensible default
  reasoning effort for the cloud preset.
- UI: a reasoning-effort selector in the provider tuning disclosure, with a note
  about the Anthropic trade-off.

**Acceptance:** effort is persisted per provider, reaches the request builders,
and is correctly shaped per provider; Anthropic falls back to `tool_choice: auto`
when thinking is on; existing constrained-decoding tests still pass.

## Phase B — Model-list sync for all providers

**Crates:** `archivist-api`, `archivist-ai`; **frontend** `ProviderModelSelect`.

- Generalize `POST /model-providers/{name}/models` (today Ollama-only):
  | Provider kind | base_url | listing endpoint |
  |---|---|---|
  | Ollama (local) | `http://…:11434` | `GET /api/tags` (existing) |
  | Ollama Cloud | `https://ollama.com` | `GET /v1/models` |
  | OpenAI | `https://api.openai.com/v1` | `GET /v1/models` + chat/vision filter |
  | OpenAI-compatible | custom | `GET /v1/models` |
  | Anthropic | `https://api.anthropic.com/v1` | `GET /v1/models` (Models API) |
- New `list_models()` client methods in `archivist-ai` for OpenAI/Anthropic and
  an OpenAI-compatible `/v1/models` path for Ollama Cloud. OpenAI results are
  filtered to chat/vision families (gpt-*, o*-families; drop embeddings, TTS,
  whisper, image, moderation).
- Validate outbound URL and inject the provider secret/API key (as the existing
  Ollama path does).
- Unified response shape `{ provider, models: [{ id, label? }] }`.
- UI: enable the sync button in `ProviderModelSelect` for **all** provider kinds;
  merge fetched IDs with the catalog (recommendation/badges from catalog,
  availability from sync; flag catalog entries missing from the live list).

**Acceptance:** sync works for each provider kind, returns a sensible filtered
list, surfaces auth/URL errors clearly, and never blocks settings editing.

## Phase C — Editable model catalog in Settings

**Crates:** `archivist-core` (+ serde defaults), `archivist-db` (none beyond JSON),
**frontend** (catalog editor + `ProviderModelSelect` reads catalog).

- New structure in `RuntimeSettings`, e.g. `ai.model_catalog: Vec<ModelCatalogEntry>`:
  ```
  ModelCatalogEntry {
    provider_kind: AiProviderKind,
    capability: text | vision,
    model_id: String,
    label: String,
    recommended: bool,
    usage_tier: Option<low|medium|high|extra_high>,
    context: Option<String>,      // e.g. "256K", "1M"
    modality: Option<String>,     // e.g. "text", "text+image"
    best_for: Option<String>,     // short use-case note
  }
  ```
- **Seed** from the matrix via a Rust default function (analogous to
  `default_providers()`). No hard DB migration needed — the catalog lives inside
  the `runtime_settings` JSON, so existing rows pick up the default via serde.
- **UI:** a catalog editor in Settings (add / edit / remove entries, set
  recommendation, usage tier, context, modality, best-for). `ProviderModelSelect`
  reads from the catalog (rendering badges) instead of the static
  `modelCatalog.ts`, which is retired once parity is reached.
- **Sync × catalog:** the sync button can append newly discovered IDs as
  un-annotated catalog entries the operator then annotates. Catalog = source of
  truth for recommendation/metadata; sync = availability reconciliation.

**Acceptance:** the catalog is editable and persisted; `ProviderModelSelect`
renders catalog-driven options + badges; sync appends new IDs; default seed
matches the matrix recommendations.

---

## Reference: Ollama Cloud default recommendations (matrix 2026-05-25)

Curated, not an official ranking. Source: Ollama Cloud Model Search,
`/v1/models`, model pages, API docs and pricing.

| Use case | Default | API-ID | Usage | Context | Input |
|---|---|---|---|---|---|
| Heavy reasoning / math / long analysis | DeepSeek V4 Pro | `deepseek-v4-pro` | Extra High | 1M | Text |
| Agentic coding / SWE | GLM-5.1 | `glm-5.1` | High | 198K | Text |
| Large codebases / repo understanding | Qwen3-Coder 480B | `qwen3-coder:480b` | High | 256K | Text |
| Vision / OCR / screenshots / GUI | Qwen3-VL 235B | `qwen3-vl:235b` | High | 256K | Text+Image |
| Multimodal allrounder | Qwen3.5 397B | `qwen3.5:397b` | Medium | 256K | Text+Image |
| Long tool agents / browsing | Kimi K2 Thinking | `kimi-k2-thinking` | High | 256K | Text |
| 1M context, better usage trade-off | DeepSeek V4 Flash | `deepseek-v4-flash` | Medium | 1M | Text |
| Office / productivity / complex workflows | MiniMax M2.7 | `minimax-m2.7` | Medium | 200K | Text |

> The matrix contains very recent / forward-looking IDs. They are adopted as
> provided; the live sync (Phase B) is what keeps the curated catalog honest
> against actual `/v1/models` availability.
