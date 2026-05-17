# Release Notes

> Versioning policy: the Git tag (`vX.Y.Z`) is the source of truth.
> `frontend/package.json` tracks the UI release alongside the tag (currently
> `1.5.15`). The Rust workspace `Cargo.toml` files remain at the pre-GA
> internal version `0.3.2`; bumping them does not change the release.

## v1.5.15 — Bundle E: Consensus + A/B prompt experiments (last bundle of v1.6.0)

Final bundle of the v1.6.0 "Prompt & Process Quality" milestone — closes
issues #118 and #119 from milestone 15. Backend-only release; UI surfaces
for both features are deferred to a follow-up. With this release every
sub-issue in the milestone is closed.

### 1. Two-model consensus for high-stakes fields (#118)

`AiSettings` gains `consensus_secondary_text_model: Option<String>`
(default `None`) and `consensus_date_tolerance_days: i64` (default 1).
When the secondary model is configured AND the runtime mode is
`full_auto` AND `dry_run` is off, `process_metadata` runs a focused
secondary call against that model asking ONLY for `correspondent` and
`document_date`. The answer is parsed via
`archivist_ai::parse_consensus_answer`.

Comparison rules:

* correspondent — case-insensitive exact match on the name. Empty
  secondary answer is "no opinion" (NOT a disagreement).
* document_date — both sides parsed as ISO; absolute day difference
  must be ≤ `consensus_date_tolerance_days`. Empty / un-parsable
  secondary answer is "no opinion".

On disagreement the disagreeing field is wiped from the primary
suggestion so it falls into review instead of being auto-applied. An
audit event `workflow.consensus_disagreement` is emitted with the
secondary model name, both candidate values, and which fields
disagreed. A `ConsensusOutcome` is also stamped into
`ai_artifacts.normalized.consensus` so dashboards can chart the
disagreement rate over time.

Operators opt in by setting `ai.consensus_secondary_text_model` to a
text model name different from the primary (e.g. primary
`qwen3-paperless:8b`, secondary `qwen3:8b`). The setting round-trips
through the existing `/api/settings` endpoint with the new fields
typed as optional in `frontend/src/api/client.ts` (a separate UI
surface for it lands in a follow-up).

### 2. A/B prompt experiment groups (#119, backend)

Migration `0024_prompts_experiment_group.sql` adds an
`experiment_group text` column to `prompts` constrained to
`{NULL, 'A', 'B'}`. The "one active per (stage, name)" unique partial
index is replaced with one that partitions by experiment_group so each
of `(NULL, 'A', 'B')` can hold one active row independently.

New DB helper `get_active_prompt_with_experiment(stage, run_id)`
returns the prompt + experiment-group label. When both `A` and `B`
rows are active, it picks deterministically with `run_id.as_u128() %
2`. With only `NULL` active, it returns the default (current
behaviour). With only `A` or only `B` active, it returns that one
labelled accordingly.

New worker helper `apply_active_prompt_with_experiment` uses it on
the metadata stage. The chosen label is stamped into
`ai_artifacts.normalized.prompt_experiment_group` so a future
dashboard panel can compute per-variant approval rates without
re-running the LLM.

Backend-only for v1.5.15: operators need to insert the `B` variant
manually (or via a follow-up Prompts UI extension) before the A/B
routing kicks in. With only the v1.5.11-seeded `metadata` default
prompt present, behaviour is unchanged.

## v1.5.14 — Bundle D: OCR cache + content-hash dedup (and the v1.5.13 clippy hotfix)

Closes issues #116 and #117 from milestone v1.6.0. Fourth of four
bundles. v1.5.13 failed CI on a `clippy::doc_lazy_continuation` lint
in the metadata-prompt doc comment; this release rolls that fix in
along with Bundle D so production skips from v1.5.12 straight to
v1.5.14.

### 1. OCR page-level cache (#116)

Migration `0022_ocr_page_cache.sql` adds an
`ocr_page_cache (paperless_document_id, page_index, page_hash,
ocr_text, provider, model, created_at)` table keyed on
`(document_id, page_index, page_hash)`.

`process_ocr` now computes `sha256(rendered_png_bytes)` for each page
and looks it up before sending the page to the vision model. Hit →
the cached text is reused; the vision model is not called. Miss →
the vision model runs and the result is cached. Hashes capture both
the rendering config and the document content, so re-renders with
different DPI/render settings get a fresh hash and fresh LLM work.

Cached pages are tracked per-job in the new
`ai_artifacts.normalized.pages_from_cache` counter, so the dashboard
can chart cache hit rate over time.

Cache writes are best-effort — a failure in the cache layer is
logged but does not fail the OCR job.

### 2. Content-hash deduplication (#117, signal-only)

Migration `0023_document_inventory_ocr_hash.sql` adds
`document_inventory.ocr_content_hash` (text, indexed).

After OCR succeeds, the worker writes
`sha256(combined_ocr_text)` to this column. When the metadata stage
starts for a document, it checks for another document with the same
content hash whose `metadata_status` is `succeeded`. On a hit, it
emits an audit event `workflow.metadata_dedup_match` with
`dedup_source = <other_document_id>` and **continues** with a fresh
LLM call.

This is intentionally signal-only for v1.5.14 — operators see in the
audit log that the system noticed a duplicate, but the metadata is
still freshly derived. A future release can flip the behaviour to a
hard skip + clone of the source patch once the hash-match approach
has proven reliable in production.

### 3. v1.5.13 clippy hotfix (rolled in)

`prompt_for_metadata`'s doc comment had a stray continuation line
that Rust 1.95's clippy flagged as `doc_lazy_continuation`. The
bullet list is now correctly closed before the v1.5.13 addition,
unblocking the CI rust:clippy job.

## v1.5.13 — Bundle C: Document-type-conditional prompts

Third of four bundles in the v1.6.0 "Prompt & Process Quality" milestone.
Closes issue #115 from milestone 15.

### How it works

For each document about to enter the consolidated metadata stage,
`process_metadata` now first runs a cheap one-shot classifier LLM call
that returns a single category word: `invoice`, `receipt`, `contract`,
`letter`, `certificate`, `notice`, `medical`, `legal`, `statement`,
`bank_statement`, or `other`. The category is then used to look up a
short hint snippet (≤ 400 chars) that is prepended to the main metadata
user prompt under a `Document-type hint:` header.

The hint snippets are domain-specific guardrails — e.g. for invoices:

> Pay special attention to: invoice number (Rechnungsnummer / Rechnung
> Nr. / Invoice #), the GROSS total (Bruttobetrag / Gesamtbetrag /
> Total), and the issue date labeled as 'Rechnungsdatum' / 'Invoice
> date' (NOT the payment-due date or delivery date). The correspondent
> is the issuer (top-of-document letterhead), not the recipient.

Eleven distinct snippets cover the canonical categories; `other` ships
an empty string so the prompt is unchanged when the classifier is
uncertain. Snippets are hardcoded in `archivist-ai`
(`metadata_hint_for_doc_type`) for v1.5.13; a follow-up will lift them
into the `prompts` table for operator-side iteration if the hardcoded
defaults prove to be the wrong starting point.

### Implementation details

* New `archivist_ai::DocTypeCategory` enum (11 variants + `parse()` +
  `as_str()`) and a `prompt_for_doc_type_classify` builder that emits
  a tight 2000-char-bounded prompt.
* New `archivist_worker::classify_document_type` helper that wraps the
  classifier call. Reuses the metadata stage's provider+model so no
  separate endpoint configuration is needed. Failures degrade
  gracefully to `DocTypeCategory::Other` with a warn-level log; the
  main pipeline keeps draining.
* `prompt_for_metadata` gained a 10th argument `doc_type_hint: &str`.
  Empty string ≡ no hint, current behaviour. The hint is prepended
  after the language context block.

### Trade-offs

* Adds one extra LLM round-trip per document (the classifier). With
  Ollama-local + qwen3-paperless:8b the classifier completes in
  ~3-8s; the main metadata call is ~30-60s, so the overhead is ~10%
  per doc.
* Classification errors fall back to `other` (empty hint) — same
  behaviour as v1.5.12.

### Test coverage

Existing test `metadata_prompt_only_requests_enabled_fields` already
covered the variable-arity prompt builder; it now exercises the
10-argument signature with an empty hint. No new failure cases.

### Out of scope (deferred to v1.5.14)

* UI surfaces for the per-field confidence thresholds and date-anchor
  settings introduced in v1.5.12 — defaults are sensible and the new
  settings round-trip through Save without UI changes, but they're
  not yet exposed for direct editing in the Settings page.
* Lifting `metadata_hint_for_doc_type` snippets into the database.
* Persistence of the classified category as a column on
  `document_inventory` so the dashboard can chart category mix.

## v1.5.12 — Bundle B: Process-quality improvements

Second of four bundles in the v1.6.0 "Prompt & Process Quality" milestone.
Three sub-issues from milestone 15: #112 (allowed-list pre-filter),
#113 (date anchor hardening), #114 (per-field confidence thresholds).

Backend-only release; matching UI surfaces for the new settings will
land with Bundle C (v1.5.13).

### 1. Per-field confidence thresholds (#114)

`MetadataSettings.confidence_threshold` is now a fallback for five new
per-field overrides: `title_confidence_threshold`,
`correspondent_confidence_threshold`,
`document_type_confidence_threshold`, `tags_confidence_threshold`,
`fields_confidence_threshold`. `document_date_confidence_threshold`
already existed and is now part of the same scheme. Defaults:

| Field | Default |
|---|---|
| title | 0.60 |
| correspondent | 0.80 |
| document_type | 0.75 |
| document_date | 0.90 |
| tags | 0.65 |
| fields | 0.80 |

`effective_<field>_threshold()` accessors return the override when
above zero, falling back to the global `confidence_threshold`. Old
configs upgraded from v1.5.11 will see 0.0 for the new fields →
graceful fall-through to the old global behavior. Operators can dial
the per-field values in the Settings UI once Bundle C ships.

### 2. Allowed-list pre-filter (#112)

`process_metadata` now calls a new `prefilter_allowed_list` helper
before building the prompt. The helper:

* Returns the input as-is when `max == 0` (disabled) or `len <= max`.
* Otherwise scores each entry by counting case-insensitive occurrences
  of the entry name in the OCR text and keeps the top-`max` by score.
* Falls back to alphabetical top-`max` if no entry has any substring
  hit, so the LLM never receives an empty list.

New setting `metadata.allowed_list_max` (default 20) controls the
cap. Eliminates the "200+ correspondents inflate the prompt by 6 KB
and dilute the model's attention" failure mode.

### 3. Date-anchor hardening (#113)

After the metadata LLM call, before validating the date suggestion,
`process_metadata` checks whether the suggested ISO date appears in
the OCR text within ±80 characters of a known anchor phrase
(Rechnungsdatum, Ausgestellt am, Invoice date, Date of issue, Date de
facturation, Data fattura, …). When it doesn't, the confidence is
reduced by `metadata.document_date_anchor_penalty` (default 0.30)
before the per-field threshold gate runs. Combined with the higher
per-field threshold for dates (0.90), this kills the common
"LLM picks up a delivery date or scan date instead of the actual
document date" failure.

Setting `metadata.document_date_anchor_required` (default true) gates
the whole check so operators can opt out if their documents don't
follow the anchor-phrase convention.

The penalty event is surfaced two ways: it's added to the
`composite_warnings` list when the date validates and applies, and as
a `ValidationError::DataQuality` row when the date drops below the
threshold and falls into review.

### Test coverage

Eight new unit tests in `archivist-core`:

* `prefilter_allowed_list_returns_full_list_below_cap`
* `prefilter_allowed_list_disabled_by_zero_max`
* `prefilter_allowed_list_keeps_substring_hits_above_alphabetical`
* `prefilter_allowed_list_falls_back_to_alphabetical_when_no_hit`
* `document_date_anchor_matches_iso_near_rechnungsdatum`
* `document_date_anchor_matches_de_format`
* `document_date_anchor_misses_when_no_phrase_nearby`
* `document_date_anchor_misses_when_date_not_present`

## v1.5.11 — Bundle A: Prompt-quality improvements

First of four bundles in the v1.6.0 "Prompt & Process Quality" milestone.
Three sub-issues from milestone 15: #109 (Metadata-prompt in DB),
#110 (few-shot examples), #111 (confidence calibration).

### 1. Metadata system prompt lifted into the `prompts` table (#109)

Migration `0021_metadata_prompt_seed.sql` inserts the consolidated
Metadata-Stage system prompt as a normal `prompts` row
(`stage='metadata'`, `name='default'`, `version=1`, `active=true`). Until
now there was no row for the consolidated stage, so
`apply_active_prompt` fell through to the hardcoded
`DEFAULT_METADATA_SYSTEM_PROMPT` constant. Operators can now edit the
prompt from the Prompts UI without a redeploy. Migration is idempotent
via `ON CONFLICT (stage, name, version) DO NOTHING`.

### 2. Confidence calibration guidance (#111)

`DEFAULT_METADATA_SYSTEM_PROMPT` (and the DB-seeded twin) now ends with:

> Calibrate confidence on this scale: 0.95 or higher only when the value
> is literally printed and unambiguous; 0.70 to 0.94 when inferred from
> clear context; below 0.70 when uncertain. Round to two decimals.

LLMs left to their own devices return 0.99 for everything; this gives
them a graded scale so downstream confidence thresholds become
meaningful.

### 3. Three German few-shot examples (#110)

`prompt_for_metadata` now embeds three concrete `INPUT (OCR) → OUTPUT
(JSON)` examples before the document text: a German invoice (DITech-
style), a medical letter (Rezept), and an official notice (FernUni
Hagen Bescheid). The examples deliberately cover only the four
high-stakes fields (title, document_type, correspondent, document_date)
and OMIT tags/fields — the shape-lines block built per call already
documents tags/fields syntax when those features are enabled, so
duplicating them in the few-shot would pollute the expected output
shape on docs with tags/fields disabled.

Confidence values in the examples follow the calibration scale,
giving the LLM a concrete demonstration alongside the abstract rule.

Expected effect: better date extraction (clear separation of
Rechnungsdatum from Versanddatum / scan date), more confident
correspondent matching, and tighter title formatting on common doc
types. To be measured against the production review-approval rate
once the v1.5.11 image rolls out.

## v1.5.10 — Inventory search-bar readability fixes

Tiny CSS hotfix on top of v1.5.9 after operator feedback:

* The search input field looked unusable — typing produced no visible
  cursor or text because the rule set in v1.5.9 stripped the caret colour
  and inherited an indeterminate text colour. v1.5.10 sets explicit
  `color: var(--text)` and `caret-color: var(--text)` and gives the
  wrapper a `:focus-within` highlight using the existing `--teal` /
  `--teal-soft` theme tokens so it's obvious when the field is active.
* The "Erweiterte Filter" and "Filter zurücksetzen" buttons used the
  `.ghost-button` style, which is the very-light sidebar-button colour
  on purpose — on the cream workspace background it was nearly
  invisible. They now use `.chip-button` (same as the preset chips),
  with the toggle reflected as the standard "active" state.

## v1.5.9 — Inventory search + filters

The Inventory page goes from a flat scrolling list of 5957 rows to a
filterable, searchable view, so operators can find the one document they
care about without paging through everything.

### Backend (`archivist-db` + `archivist-api`)

`list_inventory` and a new `count_inventory` take an `InventoryQuery`
struct and build WHERE clauses dynamically via `sqlx::QueryBuilder`.
Empty `InventoryQuery` short-circuits to the original full-table count
path so the unfiltered case stays cheap. `/api/inventory` accepts these
query-string parameters, all optional:

| Param | Meaning |
|---|---|
| `id` | Exact match on `paperless_document_id`. |
| `q` | ILIKE substring on `title` OR `original_file_name`. |
| `ocr_status`, `metadata_status`, `run_status` | Comma-separated lists; row matches any value. |
| `tag`, `not_tag` | Comma-separated tag names; AND-include and any-of-exclude. |
| `lang` | Exact match on `detected_language`. |
| `date_from`, `date_to` | Range on `document_date` (YYYY-MM-DD). |
| `has_error` | `true` requires `last_error is not null`, `false` is the inverse. |
| `needs_review` | Boolean on `document_inventory.needs_review`. |

`total` in the response reflects the filtered total so the "Showing N of M"
counter is accurate under filters.

### Frontend (Inventory page rewrite)

* **Smart quick-search bar** at the top — numeric input filters on
  `paperless_document_id`, free text filters on `q` (title +
  original_file_name).
* **Preset chips** for the four common triage cases: Failed OCR,
  Waiting for review, Has error, Missing metadata. Clicking toggles the
  underlying filter, click again to clear.
* **Advanced filter panel** (collapsible) with multi-select for the
  three status fields, CSV inputs for tag include / exclude,
  language dropdown, date-from / date-to date pickers, and checkboxes
  for has_error / needs_review.
* **URL state sync** — every active filter is serialized into
  `window.location.search` via `history.replaceState`. Bookmarks and
  shareable links work; reload preserves the filter state.
* "Showing N of M" + "Load more" continue to work and reflect the
  filtered total.
* Empty filtered result shows a "No documents match the current filters"
  row rather than a confusing blank table.

Adds ~20 new i18n keys (`inventory.search_*`, `inventory.chip.*`,
`inventory.filter.*`) across the seven supported locales.

## v1.5.8 — Opt-in Debug console with live activity feed

Adds a Debug section to the sidebar with a real-time view of what the
worker is doing right now — handy when chasing problems like the
"why is nothing happening" or "why does this document keep failing"
investigations we ran against v1.5.6 / v1.5.7.

### Settings → UI → Enable Debug console

A new opt-in toggle under Settings → UI. Off by default; flip it on and
a Debug entry appears in the left sidebar. Off, the sidebar stays
unchanged. Backed by a fresh `ui.debug_console_enabled` boolean on
`RuntimeSettings` (with a new `UiSettings` substruct).

### What the Debug page shows

Polls `/api/dashboard/live` and `/api/audit` every 2.5 s and renders
five panels:

* **Active jobs** — what the worker is currently processing
  (document, stage, status, attempts, updated-relative).
* **Active runs** — what's mid-pipeline.
* **Recent LLM events** — last provider/model calls with duration.
* **Recent failures** — most recent error messages and failure kinds.
* **Recent audit events** — last 50 audit rows (event type, actor,
  outcome, document, when).

A Pause / Resume button stops polling so the operator can read a frozen
snapshot. All keys i18n'd across the seven supported locales.

## v1.5.7 — Five fixes so `full_auto` is really hands-off

Five separate fixes shipped together because production tracing showed they
all needed to land before the autopilot promise actually held:

### 1. `complete_job` resets `pipeline_runs.status` between stages

When a stage succeeded inside a multi-stage run, `complete_job` only flipped
`pipeline_runs.status` to `succeeded` if every stage was done. Intermediate
successes (e.g. OCR done, metadata still queued in `["ocr", "metadata"]`
runs) silently left the run on `running`, which the dashboard then flagged
as "N stuck run(s) — pipeline runs have not progressed in the last 10
minutes" — even though the next-stage job WAS waiting in the queue and got
claimed normally. `complete_job` now mirrors `mark_review_auto_applied` and
flips the run back to `queued` whenever there are pending jobs left.

A one-shot startup helper `reset_stuck_running_pipeline_runs` cleans up the
386 historical residue rows: any `pipeline_runs.status='running'` with no
`jobs.status='running'` for that run is flipped to `queued` (if pending
jobs remain) or `succeeded` (if every job has settled).

### 2. `full_auto` no longer demotes one job into six review items

The consolidated `Stage::Metadata` (`process_metadata`) had a routing rule:
"if any field had a validation warning (UnknownTag, UnknownChoice,
EmptyOutput, …), demote every validated field to its own review item so the
operator inspects the full set atomically." That rule fires regardless of
`workflow.mode`, so one metadata job for a single document produced six
review items in 50ms whenever the LLM suggested a tag Paperless didn't know.
For an operator running `full_auto`, this turns the Review queue into a
manual-approval pile and explodes Paperless API calls 6×.

The branch is now gated on `!auto_apply`: in `full_auto`, the partial
`composite_patch` (whatever the validator resolved cleanly) is applied
directly to Paperless and the warnings are recorded on the job result as a
`dropped_field_count` audit trail. Manual and dry-run modes keep the
six-review behaviour unchanged. A new explicit branch also handles the
edge case of "every field had a warning, nothing resolved in `full_auto`":
the job completes with a `skipped` result instead of dropping through the
"all skipped (already-set)" branch with a misleading message.

### 3. Vision-fallback runtime safety net widened, `num_ctx` raised to 32768

Production observed 137 OCR jobs burning through their retry budget despite
v1.5.1's `num_ctx=16384` floor — that ceiling is still too small for some
multi-page or high-DPI renders, and the auto-discovered vision fallback
chain only knew about `qwen2-vl:7b` / `llava-*` (not installed in this
deployment).

* `bump_vision_num_ctx_if_too_small` (one-shot, startup) raises
  `ai.ollama_vision_num_ctx` from any value ≤ 16384 to 32768. Operators
  who already manually raised it are left alone.
* The auto-discovery chain (`VISION_FALLBACK_CHAIN`) now also includes
  `qwen2.5vl:7b`, `qwen3-vl:32b`, so the runtime fallback fires for the
  models actually installed in modern Ollama deployments without operators
  having to set `ai.fallback_vision_model` manually.
* The existing `requeue_vision_crashed_jobs` startup helper picks up the
  137 burned-out OCR jobs at next worker boot and gives them another
  attempt under the raised `num_ctx` ceiling.

### 4. Inventory: separate "Trigger metadata" button

The Inventory row now exposes three trigger buttons: **OCR** (FileText),
**Metadata** (Tags), **Run full pipeline** (Sparkles). Previously
metadata-only triggers required running the whole pipeline (which would
redo OCR work). Useful when an operator wants to re-run only the LLM
metadata extraction on a document whose OCR text is already cached.

### 5. Inventory: pagination with "Load more"

`/api/inventory` now returns `{items, total, offset, limit}` and the
frontend defaults to fetching 500 at a time with a "Showing N of M
documents" counter and a "Load more" button. Previously the page silently
showed only the first 200 of the ~6000 production documents.

UI sidebar reads `v1.5.7`.

## v1.5.6 — Hot-fix the backfilled metadata-job priority

Follow-up correction to the v1.5.4 metadata-stage backfill. The backfill
priced every newly inserted metadata job with
`payload.priority = 1_000_000 − paperless_document_id` (~993 000–999 999),
but the historical trigger-polling OCR jobs sit at `payload.priority = 10`.
`claim_jobs` orders by `priority ASC, stage_priority ASC`, so the
backfilled metadata jobs could not be claimed until *every* OCR job in the
entire queue was succeeded — even for runs whose own OCR was already done.
In production, this left all 5953 metadata jobs queued indefinitely behind
the OCR backlog, the exact opposite of the "full_auto completes every
document" promise of v1.5.4.

Two changes:

* `backfill_metadata_stage_for_ocr_only_runs` now sets the new metadata
  job's `payload.priority` by INHERITING the sibling OCR job's priority
  for the same `run_id` instead of computing a fresh `1M − doc_id`. The
  `stage_priority = 20` is unchanged and still guarantees OCR-before-
  metadata ordering within the run; the corrected `priority` keeps the
  cross-run ordering exactly as the operator who queued the run intended.
* A new one-shot `rebalance_backfilled_metadata_priorities` runs on
  worker startup, finds every still-queued metadata job whose
  `payload.backfill = true` and whose `payload.priority` disagrees with
  its OCR sibling, and rewrites the priority to match. Idempotent —
  subsequent startups find nothing to do once every backfilled job is
  rebalanced.

After v1.5.6 rolls out, the worker resumes claiming metadata jobs in
interleaved order with OCR jobs (sorted by the run's original
operator-intended priority and the per-run stage order), draining the
combined OCR+metadata backlog at the realistic two-jobs-per-tick concurrency
the worker is configured for.

UI sidebar reads `v1.5.6`. No frontend behaviour change.

## v1.5.5 — Inventory page reflects the v1.4 stage model

The Inventory table still rendered four columns (`Tags`, `Titel`, `Typ`,
`Datum-Status`) backed by the legacy per-field inventory status columns
(`tagging_status`, `title_status`, `document_type_status`,
`document_date_status`). The v1.4.0 consolidation replaced those six
stages with a single `Stage::Metadata`, and the worker stopped writing to
the legacy columns. The columns therefore showed `unknown` for every one
of the production deployment's 5957 documents — five years of legacy UI
dead weight.

This release:

* Exposes the consolidated `metadata_status` field on
  `DocumentInventoryItem` (`crates/archivist-core/src/lib.rs`) and on
  `list_inventory` (`crates/archivist-db/src/lib.rs`) so the API actually
  surfaces it.
* Rebuilds the Inventory table: drops the four dead per-field stage
  columns, adds one `Metadata` column backed by `metadata_status`, and
  changes the `Tags` column from showing a dead stage status pill to
  showing the actual current Paperless tags (`current_tags`). The
  `Datum` column shows the raw `document_date` value instead of a stage
  status pill.
* Renames the row's second action button from "Trigger tagging" (which
  queued a legacy `tags` stage that no longer runs as a top-level job) to
  "Run full pipeline" — it now triggers `['ocr', 'metadata']` together,
  the safe default when an operator wants a complete re-process.
* Adds `inventory.metadata` and `inventory.trigger_pipeline` keys to all
  seven UI locales.

No application behaviour change for runs themselves; this is an inventory-
display correction and a renamed manual trigger. UI sidebar reads `v1.5.5`.

## v1.5.4 — Full Auto really completes every document

Closes the gap between what `workflow.mode = full_auto` promised and what was
actually shipping to Paperless. Four changes ship together:

### 1. Backfill the consolidated `metadata` stage onto OCR-only pipeline runs

Historical `pipeline_runs` queued by trigger polling against documents tagged
only with the OCR trigger were created with `stages = ["ocr"]` — they
terminated after OCR with no `Title` / `Correspondent` / `Tags` /
`DocumentType` / `Date` suggestions ever produced. The Review queue filled up
with content-only review items that the operator could not meaningfully act
on. On worker startup the new
`archivist_db::backfill_metadata_stage_for_ocr_only_runs` (idempotent,
single-transaction) finds every such run, appends `metadata` to its `stages`
array, inserts a queued `metadata` job behind the existing OCR job (using
`stage_priority = 20` so it sequences after OCR), and flips already-finished
runs back to `queued` so the worker re-picks them up. After the first
successful pass, subsequent startups find nothing to do.

### 2. Autopilot review drain runs off the main tick loop

`drain_pending_reviews_if_autopilot_tick` used to be `.await`-ed on the
worker's 5-second tick loop. A drain of 100 items at ~5s of Paperless API
latency each took ~8 minutes, during which OCR job processing was completely
starved. The drain is now spawned via `tokio::spawn` with an atomic re-entry
guard (mirroring the trigger-polling pattern), so the main loop keeps
claiming and processing OCR jobs in parallel.

`PER_TICK_CEILING` is also bumped from 100 → 500, and the outer drain
timeout from 8 → 30 minutes to match. Sustained throughput against a 2515-
item backlog moves from ~140 items/hour to roughly an order of magnitude
higher, bounded only by Paperless API write latency.

### 3. Real Paperless title in the Review queue

`list_reviews` now joins `document_inventory.title` and surfaces it as
`paperless_title` on every `ReviewItem`. The Review card prefers it over the
generic `Document {id}` fallback (which it falls back to only when the
inventory has no cached title for the document yet).

### 4. Selector pill no longer renders literal `Selector unknown`

When the server-side `debug_context` did not include `selector_reason` or
`next_required_stage` — the common case for review items, since those fields
describe the auto-selector decision, not why a review exists — the frontend
fell back to the literal English word "unknown" embedded inside the
translated "Prompt-Sprache de; Tag-Sprache de; Selector …" pill. The pill
now uses a separate i18n template (`review.debug_summary_no_selector` /
`inventory.debug_summary_no_selector`) that omits the selector segment when
no meaningful value is available, and the corresponding `<dl>` row is
hidden too. New keys added to all seven UI locales.

### Heads up for operators on upgrade

* The backfill runs once on the first v1.5.4 worker boot. Expect a one-line
  `metadata-stage backfill lifted OCR-only pipeline_runs to include the
  metadata stage` log entry with `runs_updated` and `jobs_inserted` counts.
* Documents whose OCR was already `succeeded` will have their run flipped
  back to `queued` so the worker can pick up the new metadata job — the
  dashboard "succeeded" badge for those documents will briefly drop and then
  climb back as metadata runs.
* No application behaviour change for fresh installs. UI sidebar reads
  `v1.5.4`.

## v1.5.3 — Apply Debian Security patches in the runtime image

The runtime stage of `Dockerfile` now runs `apt-get upgrade` so every build
pulls the current Debian Security patches for libraries that ship
pre-installed in `debian:bookworm-slim` and are otherwise frozen at whatever
version Docker Hub baked into the base tag.

Without this, image scans (Trivy) flagged CVEs that Debian had already fixed
upstream — observed examples: CVE-2026-0861 (`libc-bin` / `libc6`),
CVE-2026-4878 (`libcap2`), CVE-2026-29111 (`libsystemd0` / `libudev1`). The
patched versions were available in the Debian Security mirror; we just
weren't pulling them.

The fix is a one-line addition to the runtime layer:

```dockerfile
RUN apt-get update \
  && apt-get -y --no-install-recommends upgrade \
  && apt-get install -y --no-install-recommends ca-certificates curl poppler-utils \
  && rm -rf /var/lib/apt/lists/*
```

Multi-stage build stages (`rust:1.95-bookworm`, `node:26-bookworm`) are
unchanged — only the compiled binaries and the frontend dist are copied into
the runtime image, so their base libraries never ship.

No application behaviour changes. UI sidebar reads `v1.5.3`.

## v1.5.2 — Pipeline-run + tag-resolution fixes

Two surgical fixes on top of v1.5.1:

- `queue_full_batch` now queues a single full pipeline run per document
  covering all enabled stages (`["ocr", "metadata"]`) instead of N
  single-stage runs.
- Tag names and custom-field names emitted by the metadata stage are
  resolved to integer IDs before being stored in `review_items`, so
  `POST /api/reviews/{id}/approve` no longer 500s with
  `invalid type: string "<name>", expected i32`.

## v1.5.1 — Root-cause fix for glm-ocr GGML_ASSERT crashes

Pins Ollama's `options.num_ctx` on vision and text calls so the configured
primary vision model (glm-ocr by default) stops crashing on realistic
document pages. This is the **root-cause** fix for the GGML_ASSERT runtime
crash that the v1.5.0 fallback machinery had to paper over.

### What was crashing

Vision runs against `glm-ocr` (or any vision model that expands a page into
many thousands of vision tokens) were aborting Ollama's llama runner with:

```
GGML_ASSERT(a->ne[2] * 4 == b->ne[0]) failed
llama runner process no longer running: 2 error: ...
```

Upstream confirmed in [ollama/ollama#14401][upstream-14401] and
[ollama/ollama#14171][upstream-14171] that the assertion fires when the
vision-token count for a page exceeds Ollama's context window. Ollama's
built-in default is **4096 tokens**, which is too small for a realistic
single-page render. Upstream user `hapm` confirmed: "Context size was
configured to 7000, works well with 8192."

[upstream-14401]: https://github.com/ollama/ollama/issues/14401
[upstream-14171]: https://github.com/ollama/ollama/issues/14171

### The fix

- The worker now wires `options.num_ctx` into every Ollama vision and text
  payload. The default for vision is **16384** (safe ceiling for commodity
  hosts, headroom for multi-page rendering at high DPI). The default for
  text-chat is **8192** (covers the 16k-char metadata-extraction prompt
  with comfortable headroom).
- Remote providers (OpenAI / Anthropic / OpenAI-compatible) ignore the
  field — the override only travels to the local Ollama runner.
- Operators can re-tune both numbers from the Settings → AI section.
  Memory-constrained Ollama hosts can lower them; very-high-DPI multi-page
  scanners can raise them.
- All seven locales (en/de/fr/es/it/nl/pl) ship the new labels and hints.

### Defense in depth (carried over from v1.5.0)

The v1.5.0 fallback machinery stays in place:

- **Crash detection** — `is_vision_model_runtime_crash` still recognises
  the GGML_ASSERT / "runner process no longer running" signatures and
  retries the page on a fallback model (operator's `fallback_vision_model`
  setting or a hardcoded safe-default chain).
- **Startup requeue** — `run_startup_vision_crash_requeue` still lifts
  pre-fix `failed` OCR jobs back into the queue on worker boot. Because
  the requeue clears the matching `error_message`, it is naturally
  idempotent — subsequent restarts do not double-fire.

The **expected behavior after v1.5.1** is that this fallback machinery
becomes dormant: `vision_model_fallback_used=true` counts trend toward
zero, and primary glm-ocr completes without crash.

### What to watch in production after deploy

- Worker startup log line:
  `setting vision options.num_ctx and text options.num_ctx for Ollama calls`
  with the configured values. If you see 16384 / 8192, the fix is live.
- `vision_model_fallback_used=true` log count should **drop toward zero**.
  The fallback existed to mask the crash; the crash should no longer fire.
- Previously-dead OCR jobs killed by the GGML_ASSERT signature are
  automatically requeued on worker startup (one-shot) and should now
  succeed on the first attempt without a fallback hop.
- Operator does nothing. Full-Auto stays Full-Auto.

### Compatibility

- New settings (`ai.ollama_vision_num_ctx`, `ai.ollama_text_num_ctx`) carry
  `#[serde(default = ...)]` so existing `RuntimeSettings` rows deserialize
  without migration.
- `ChatRequest.num_ctx` and `VisionRequest.num_ctx` are new optional fields
  with `#[serde(default)]`. Existing API consumers that build these structs
  manually need to add `num_ctx: None` (compiler-enforced in the workspace).

## v1.4.1 — Migration compatibility fix

- Fixes migration `0019_metadata_stage.sql` by making
  `jobs.stage_priority` a stored generated column. PostgreSQL 18 does not
  support indexes on virtual generated columns.
- Extends the PostgreSQL 18 migration smoke test to assert that
  `jobs.stage_priority` is stored before a release can pass validation.

## v1.4.0 — Consolidated metadata stage + age-derived job scheduling

Two coupled architectural changes — the biggest single feature shipped in
v1.x. The pipeline default sequence becomes `Ocr -> Metadata` (replacing six
per-field stages), and the worker drains newer documents first with manual
triggers jumping the queue.

### Headline changes

**Consolidated metadata stage**

- New `Stage::Metadata` runs ONE LLM call that yields up to six fields —
  title, document_type, correspondent, document_date, tags, custom fields.
- Net effect on an end-to-end run: ~6x fewer LLM round-trips, ~5x less total
  token spend (one system+context prompt rather than six), drastically lower
  wall-clock latency per document.
- The six legacy per-field stages (`Title`, `DocumentType`, `Correspondent`,
  `DocumentDate`, `Tags`, `Fields`) remain in the `Stage` enum and stay
  selectable for prompt-management UX; in-flight runs queued before v1.4.0
  continue to drain through those code paths unchanged.
- Operators can still opt out of individual fields via
  `WorkflowSettings::enabled_stages` — the consolidated prompt builder reads
  the list and omits disabled fields from both the requested-key set and
  the closed-vocabulary allowlists.

**Age-derived priority scheduling**

- `jobs.payload` now carries TWO priority values:
  - `priority` — cross-run ordering (smaller wins). Manual triggers stamp
    `0`; the auto-selector / paperless ingest delta-sync / `queue_missing_*`
    bulk path stamps `1_000_000 - paperless_document_id` so a fresh scan
    drains its full pipeline ahead of older queued documents.
  - `stage_priority` — within-run stage ordering (smaller wins). Preserves
    the OCR -> Metadata -> ... order inside a single run regardless of the
    cross-run priority value.
- `claim_jobs` orders by `priority, stage_priority, run_after, created_at`
  and uses `stage_priority` in the within-run dependency subquery, so the
  two roles are cleanly split.
- The "Trigger OCR" / "Trigger Tags" / Reviews "Re-queue" UI buttons emit
  priority 0 so an operator-initiated action always jumps ahead of the
  backlog.

### Compatibility & backward-compat policy

- `Stage::all_business_stages()` now returns `[Ocr, Metadata]`. Existing
  rows in `pipeline_runs.stages` are NOT migrated; the worker keeps
  matching the legacy variants and produces review items as before.
- Migration `0019_metadata_stage.sql`:
  - adds `document_inventory.metadata_status` (default `'unknown'`),
  - adds `jobs.stage_priority` as a stored generated column derived from
    `payload->>'stage_priority'` with a fallback to the legacy
    `payload->>'priority'` so pre-existing rows preserve their original
    stage ordering. It is stored because PostgreSQL 18 does not support
    indexes on virtual generated columns.
- Frontend `Stage` union, `defaultStageStatus`, `promptStageOrder`, and
  Reviews per-field renderer all gain a `metadata` entry. All seven
  completeLocales (en/de/fr/es/it/nl/pl) ship `stage.metadata`.

### What to watch in production after deploy

- Dashboard StageMatrix should grow a new "Metadata" row that accumulates
  throughput as new runs drain. Legacy rows (Title, Tags, ...) should
  trend toward zero as in-flight runs finish.
- A bulk re-scan or manual trigger should observe a drop in the per-doc
  wall-clock by roughly 5-6x compared to v1.3.x.
- Verify priority scheduling: trigger a manual run on a low doc id while a
  large auto-selector backlog is queued. The dashboard live timeline should
  show the manual document drain ahead of the auto-selected ones.

### Upgrade notes

- PostgreSQL 18 or newer (unchanged).
- Stop workers, run the API to apply migration `0019_metadata_stage.sql`,
  start workers.
- No backfill required for `document_inventory.metadata_status` — the
  selector consults both the consolidated column and the legacy per-field
  columns until v1.5.

## Milestone #14 — Post-v1.1 hardening (closed)

All 25 hardening issues are landed. Highlights:

- Backend perf and safety: audit-event indexes (#80), deduped dashboard
  helper queries (#81), `queue_missing` SQL LIMIT push-down, snapshot
  off the read path (#97), bounded `provider_usage` joins (#99), typed
  SQL allowlists for status counts and stage-keyed queries (#91).
- Security: constant-time CSRF token comparison and threat-model docs
  (#83), explicit request body size limits with per-route overrides
  (#87), login IP rate limiter, SSRF URL validator, recovery permission
  alignment surfacing `permissions.read_runs` / `permissions.write_runs`
  on `/auth/me` (#98), prompt-injection threat model and cookie-secure
  default documentation (#100).
- Worker: retry backoff jitter (#88), O(1) tag lookup (#92), typed
  error variants (`PaperlessError`, `AiProviderError`) replacing the
  bulk of substring-based failure classification (#100).
- Frontend: shared ErrorBoundary at shell/tab/dashboard layers (#82),
  App.tsx extraction (Settings/Prompts/Audit/Users/DocumentChat code
  splits), inventory and reviews row memoisation, dashboard sparkline
  HashMap lookups (#100), real a11y fix in the dashboard stage matrix
  (caught by the new render test).
- Testing & tooling: pure dashboard helpers extracted and unit-tested
  in archivist-db; vitest + jest-axe coverage for `computeHealthScore`,
  `parseDocumentIds`, the review patch helpers, and shell-level axe
  assertions for `<Dashboard>`, `<Reviews>` and `<SettingsPage>` (#101).
  Informational `pnpm i18n:check` script reporting untranslated DE
  values (#100).
- Docs: ADR-010 on snapshot-bucket trade-offs, SECURITY_DESIGN.md
  section 4.2 (cookie Secure flag) and 14.1 (prompt-injection threat
  model).

## v1.1.2

- Workflow card stack layout fix on the operations strip.
- HealthBadge wrap fix and per-provider sparkline data wired up from
  bucketed series.
- Chart-pattern fills and a proper tablist under 1100 px viewport.
- Frontend a11y smoke test (axe-core) wired into the static check.

## v1.1.1

- Apply `rustfmt` to dashboard enrichment code so the workspace
  formatting check stays green.

## v1.1.0 — Operations Dashboard Overhaul (Milestone #13)

Operations-first refresh of the dashboard.

- AlertsBar with severity grouping and quick links to recovery actions.
- HealthBadge consolidating Paperless, providers, and worker liveness.
- StageMatrix with per-stage status, throughput, and failure rates.
- CostPanel with provider, model, and time-range breakdowns; cost is
  surfaced as `Option` (no fabricated zeros).
- MaintenanceDrawer for safe, low-traffic operator actions.
- A11y pass on dashboard pills, tabs, and chart fallback contexts.
- Renamed `frontend/package.json` to `1.1.0` (now `1.1.2` after the
  v1.1.1 and v1.1.2 follow-ups above).

## v1.0.0 GA

Paperless Archivist v1.0.0 is the first GA-ready release of the secure AI
automation layer for Paperless-ngx.

### Major Capabilities

- Rust API and worker with PostgreSQL 18 storage.
- React + TypeScript frontend.
- Paperless REST API integration only; no direct Paperless database writes.
- OCR, title, correspondent, document type, document date, tag, and custom-field
  extraction stages.
- Review mode, auto-select with review, and full autopilot.
- Completion tags and trigger-tag cleanup.
- Document inventory, backlog dashboard, live processing status, and recovery
  tools.
- Document Chat/RAG with citations to Paperless documents.
- UI-managed runtime settings, model providers, local Ollama model discovery,
  prompt workbench, users, sessions, and scoped API tokens.
- Local login, Argon2id, sessions, CSRF, RBAC, OIDC SSO, audit log, secret
  redaction, encrypted secret references, and audit integrity checks.
- Hardened Docker Compose profiles and generic Kubernetes package.

### Upgrade Notes

- PostgreSQL 18 or newer is required.
- Stop workers before upgrading.
- Back up PostgreSQL and `ARCHIVIST_SECRET_KEY`.
- Start the API first and wait for migrations/readiness.
- Start workers after the API is healthy.
- Run Paperless consistency check after upgrade.

### Rollback Notes

Rollback to an older version after migrations requires restoring a database
backup from before the upgrade. Do not run older binaries against a newer schema
unless that release explicitly documents compatibility.

### Known Limitations

- Non-English UI languages beyond English and German use the English text
  fallback until translated catalogs are added.
- Public Kubernetes manifests are generic and must be patched for the target
  cluster, secrets, image registry, ingress, and storage policy.
- Benchmark results are synthetic and should be repeated on the operator's
  PostgreSQL storage for very large archives.
