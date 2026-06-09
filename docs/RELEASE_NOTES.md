# Release Notes

> Versioning policy: the Git tag (`vX.Y.Z`) is the source of truth.
> `frontend/package.json` tracks the UI release alongside the tag (currently
> `1.5.35`). The Rust workspace `Cargo.toml` files remain at the pre-GA
> internal version `0.3.2`; bumping them does not change the release.

## Unreleased

### Token-usage redaction destroyed provider token statistics (#246)

Releases up to v1.11.2 redacted numeric `usage.*` token counts in stored AI
artifacts to the string `"[REDACTED]"` (and, in `redacted` storage mode,
summarized `prompt_tokens` / `prompt_eval_count` into objects). Token counts
recorded for OpenAI-compatible / Anthropic providers during that window are
unrecoverable — usage and cost statistics report 0 input/output tokens for
those historical rows. New artifacts keep numeric counters in every storage
mode, and all aggregate queries now guard the casts so the legacy rows
aggregate as 0 instead of failing the Statistics page and dashboard with
`22P02`.

## v1.5.35 — Refresh the seeded `fields` system prompt to the v1.5.28 redesign

Second instance of the same drift v1.5.34 fixed for metadata: the
seeded `fields`-stage system prompt in the `prompts` table was frozen
at the v1.5.x run-on-sentence baseline, so my v1.5.28 tightening
(numbered `<rules>` block, explicit "document labels are not
substitutes" negation, named negative-example labels like
"Rechnungsnummer" / "Police Nr." / "Versicherte(r)") was dead code on
every cluster that had ever taken the original seed migration.

A hash-compare across all stored prompts after v1.5.34 turned up:

```
ocr            OK    (DB matches code)
ocr_fix        OK
tags           OK
title          OK
correspondent  OK
document_type  OK
fields         DRIFT — DB has v1.5.x baseline, code has v1.5.29
metadata       OK    (just fixed by v1.5.34)
```

Migration 0027 selectively refreshes the seeded `fields` / `default`
row when its content still starts with the v1.5.x baseline. Operator-
customised prompts and any other versions are untouched. Idempotent on
re-runs.

Production was patched in-place via direct SQL UPDATE before the tag
to converge the running cluster immediately; the migration ships so
fresh installs and DB restores converge automatically.

OCR / OCR-fix / tags / title / correspondent / document_type prompts
all match between code and DB — no further drift to chase.

### Architectural follow-up

This is the second time in two days that a code-side prompt
improvement landed without changing what the worker actually emits.
The root cause is real: `apply_active_prompt_with_experiment`
overwrites `request.system_prompt` with the DB content, and the seed
migrations are `ON CONFLICT DO NOTHING` so any row that exists
freezes. A future release should add either:

* a Prompts-UI button "Reset to current default" that overwrites
  the active row with `DEFAULT_*_SYSTEM_PROMPT` at click time, or
* a startup check that logs a warning when the active prompt for a
  stage doesn't match the code default (so the drift is visible
  before it becomes a six-day-old bug).

## v1.5.34 — Refresh the seeded metadata system prompt to the v1.5.29 redesign

The v1.5.29 prompt redesign rewrote `DEFAULT_METADATA_SYSTEM_PROMPT`
into a clean XML-structured form with a numbered `<rules>` block, an
explicit null-fallback directive, and named negative examples for the
custom-fields binding. The new constant was live in the binary from
v1.5.29 onwards (and especially from v1.5.33 when the deploy chain
finally unblocked), but production wasn't using it.

The reason: migration `0021_metadata_prompt_seed.sql` seeded the
v1.5.11-era system prompt into the `prompts` table on every fresh
deploy so operators can edit it via the Prompts UI without a backend
change. The seed runs `ON CONFLICT (stage, name, version) DO NOTHING`,
so on every cluster that had ever taken the v1.5.11 migration the
seeded row was already there with the old text — frozen since
2026-05-17 on this prod cluster — and the v1.5.29 code redesign never
made it into the wire payload. `apply_active_prompt_with_experiment`
hard-overwrites `request.system_prompt` with the DB content; the code
constant is only the seed value.

Migration 0026 selectively updates the seeded row when its content
still matches the v1.5.11 verbatim text (the active prompt is keyed by
`stage='metadata' AND name='default' AND version=1` and only refreshed
if `content LIKE 'You are the consolidated metadata extractor for a
Paperless-ngx archive.%'`). Operator-customised prompts — different
name, different version, or content already edited away from the
v1.5.11 baseline — are untouched. Idempotent on re-runs.

The full v1.5.27–v1.5.31 stack is finally complete in production:
quota-aware backoff, dashboard unblock action, prompt redesign
(user-prompt half was already live, this completes the system-prompt
half), and constrained decoding across Ollama / OpenAI / Anthropic.

### Follow-up

A future release should add a "Reset to current default" affordance
in the Prompts UI so operators can pull in a new in-code default
without an SQL migration. Out of scope here — this release converges
the running cluster, future fresh installs converge automatically.

## v1.5.33 — Drop non-IMMUTABLE partial index in migration 0025

Second CI-gate fix on top of v1.5.32. After `rust:fmt` passed, the
`rust:migration-smoke` test failed against a fresh PostgreSQL 18
database with `functions in index predicate must be marked IMMUTABLE`.
The offending index was the partial `ai_provider_cooldowns_active_idx`
introduced in v1.5.27's migration 0025: `WHERE cooldown_until > now()`.
`now()` is STABLE, not IMMUTABLE, and Postgres rejects index
predicates that aren't deterministic at index-creation time.

The index is dropped from the migration. The `ai_provider_cooldowns`
table holds one row per configured AI provider (typically ≤5), so the
primary-key lookup covers the hot path (`get_active_provider_cooldown`
for a given provider) and the sequential scan in
`list_active_provider_cooldowns` is cheaper than maintaining a
secondary index would have been. No functional change to the
quota-aware backoff feature.

Migration 0025 was edited rather than overlaid with a new 0026 because
no production deployment ever successfully applied it — every tag from
v1.5.27 through v1.5.31 failed CI before reaching the deploy step.

## v1.5.32 — Rustfmt drift fix that unblocks v1.5.27–v1.5.31 deploys

No functional change. The five tags v1.5.27 through v1.5.31 all failed
the `rust:fmt` CI gate (`cargo fmt --all -- --check`) because of
formatting drift introduced in those changesets — long `try_get` chains
and a misplaced `use chrono` import line. Production therefore stayed
on v1.5.26 for three days while five feature releases sat unbuilt:
quota-aware backoff for Ollama Cloud, the dashboard "Entsperren"
action, the full prompt redesign, constrained decoding for Ollama, and
constrained decoding for OpenAI + Anthropic. v1.5.32 is the
single-commit `cargo fmt --all` fix that gets all of them through CI
in one go. Re-verify the deploy on this tag; everything else listed
below from v1.5.27 onward ships with it.

## v1.5.31 — Constrained decoding for OpenAI-compatible and Anthropic providers

Closes the v1.5.30 follow-up. The constrained-decoding work landed for
Ollama in v1.5.30 but explicitly deferred OpenAI and Anthropic because
their strict-mode contracts differ in subtle ways. v1.5.31 wires both
of them up so the same `response_schema` field on `ChatRequest` now
gives hard guarantees across all three providers the worker can talk
to.

### Schema shape: top-level keys now `required` + nullable

`schema_for_metadata` was non-strict at the top level (no `required`)
in v1.5.30 because Ollama's GBNF grammar doesn't need it. OpenAI's
strict mode does: every property in `properties` MUST appear in
`required`, otherwise the request is rejected with a 400. The schema
now lists every enabled top-level key in `required`, and each
property's `type` is the union `["object", "null"]` so the model
satisfies the contract by emitting `null` for keys without evidence.
This single shape works for all three providers:

* Ollama doesn't care whether keys are required and accepts the
  union type — the GBNF grammar generated from the schema enforces
  enum constraints just as before.
* OpenAI strict mode wants `required` + the union type, both
  present.
* Anthropic's tool input_schema accepts the same shape; the model
  emits `null` for unknown values.

The prompt's "It is always better to return null, [], or omit the
key than to invent a value" wording stays — when the schema forces a
key to be present, the model fills in `null` instead of omitting,
which is functionally identical for the worker's parsers.

### OpenAI-compatible wire support

The `OpenAiCompatibleClient` now reads `response_schema` and wraps it
in the canonical Structured Outputs envelope:

```json
{
  "response_format": {
    "type": "json_schema",
    "json_schema": {
      "name": "metadata_extraction",
      "strict": true,
      "schema": <schema>
    }
  }
}
```

`strict: true` activates the harder guarantees (every property in
`required`, `additionalProperties: false` honoured at every level,
no out-of-vocabulary enum values). The `name` field is a free-form
identifier surfaced in OpenAI's dashboards — we use
`metadata_extraction` so audit-log readers can grep for it. Wire-
shape testing pins the envelope so a future refactor can't silently
drop `strict: true` and degrade to plain JSON mode.

### Anthropic wire support: forced tool-use

Anthropic doesn't have OpenAI's Structured Outputs feature; the
canonical equivalent is forced tool-use. When `response_schema` is
set on a `ChatRequest`, the Anthropic client switches the payload
from a plain `/messages` call to:

```json
{
  "tools": [{
    "name": "emit_metadata",
    "description": "...",
    "input_schema": <schema>
  }],
  "tool_choice": { "type": "tool", "name": "emit_metadata" }
}
```

The model can then only respond by "calling" the tool, which means
emitting a `tool_use` content block whose `input` matches the
schema — Anthropic's structured-output equivalent.

The response parser detects the structured mode and pulls
`content[].tool_use.input` instead of `content[].text`,
JSON-serialising it back to a string so the downstream
`parse_metadata_suggestion` (and per-stage variants) work
unchanged. Defensive: if the model somehow ignores `tool_choice`
and produces a text block, the parser returns `""` and the
worker translates that into "no fields recognised" rather than
panicking.

### Effect

Every provider now sees the same closed-vocabulary constraints with
sampler-level enforcement. The `UnknownChoice` cohort that
triggered the v1.5.28 → v1.5.30 work is now blocked at the wire on
all three: Ollama via GBNF, OpenAI via Structured Outputs strict
mode, Anthropic via input_schema enum constraints in forced tool-
use. A model that's never seen the document still can't emit a
`fields[].name` that isn't in the allowlist.

### Tests added

* `openai_chat_payload_wraps_response_schema_in_strict_json_schema_envelope`
  and `openai_chat_payload_omits_response_format_when_schema_unset`
  pin the OpenAI wire shape (envelope structure + `strict: true`).
* `anthropic_chat_payload_switches_to_forced_tool_use_with_schema` and
  `anthropic_chat_payload_omits_tools_when_schema_unset` pin the
  Anthropic wire shape (single tool, tool_choice forced).
* `anthropic_response_parser_pulls_structured_input_from_tool_use_block`
  and `anthropic_response_parser_returns_empty_string_when_no_tool_use_present`
  pin the response-parsing path that turns Anthropic's tool_use into
  a JSON string the worker can feed to its existing parsers.

### Not yet (deliberate)

* **Vision-stage constrained decoding** (`VisionRequest`) — OCR output
  is free-form text by design; not a structured-output candidate.
* **Other Anthropic models that don't support tool-use** (older claude
  pre-1.0). Production runs on `claude-3-5-sonnet-latest` and similar
  recent models that all support tool-use. If an operator configures
  an older model the request still goes out — Anthropic will reject
  it with a clear error which the worker classifies as permanent.
* **Per-stage standalone prompts** (`prompt_for_fields`,
  `prompt_for_tags`, etc.) — these don't currently set
  `response_schema` either, so they continue on prompt-only steering
  even on Ollama. A separate refactor pass can add per-stage
  schema-builders parallel to `schema_for_metadata` if those paths
  become hot.

## v1.5.30 — Constrained decoding for the consolidated metadata extractor (Ollama)

Closes the soft-constraint slippage that the v1.5.29 prompt redesign
identified but couldn't eliminate. Even with the strongest XML-section
markup, allowed-list framing, and few-shot reinforcement, soft prompt
constraints leave 1–10% of responses outside the closed vocabularies
(Anthropic + OpenAI + Gemini guides all flag this). Constrained
decoding closes the gap entirely by enforcing the schema during token
sampling.

### What changed on the wire

`ChatRequest` grows an optional `response_schema: Option<Value>` field.
When set, the Ollama client forwards it as the `format` field of
`/api/chat`; llama.cpp lowers the schema to a GBNF grammar and applies
it during sampling, so out-of-vocabulary tokens become impossible to
emit. The OpenAI-compatible and Anthropic clients ignore the field for
now — they continue on prompt-only steering. Adding
`response_format: json_schema` for OpenAI and forced tool-use for
Anthropic is tracked as a follow-up; both are non-trivial because their
strict-mode constraints don't match our "keys are optional / values
may be null" contract verbatim.

### Schema mirrors the prompt

A new `schema_for_metadata` function consumes the same inputs as
`prompt_for_metadata` and produces a JSON Schema matching the
`<output_schema>` block exactly. The worker calls both in lockstep so
the prompt's textual description and the sampler's hard constraint
stay aligned. Soft prompt constraints stay in place (belt and
suspenders): a model that understands *why* a value is allowed
performs better than one that only finds out at the sampler.

The schema is deliberately **non-strict on the top-level keys** — none
of `title`, `document_date`, `document_type`, `correspondent`, `tags`,
`fields` is listed in `required`, and every key allows `null` as a
value. That matches the prompt's "omit any key whose evidence is
missing" contract. Within each present key the inner objects ARE
strict (`additionalProperties: false`, all inner shape fields
required) so a malformed object can't sneak past.

Closed-vocabulary fields carry `enum` constraints from the runtime
allowlists:

* `document_type.name` → `enum: [<allowed_document_types>]`
* `correspondent.name` → `enum: [<allowed_correspondents>]`
* `tags.tags[]` → `enum: [<allowed_tags>]` (items), `maxItems` cap
* `fields.fields[].name` → `enum: [<allowed_custom_field_names>]`, `maxItems` cap
* `tags.new_tags` → `maxItems: 0` (prompt already forbids new tags;
  schema enforces it)

Empty allowed lists collapse to `{"type": "null"}` for single-valued
keys (the model can only emit `null`, never an invented value) and
`maxItems: 0` for array-valued keys.

`document_date.date` carries `pattern: "^[0-9]{4}-[0-9]{2}-[0-9]{2}$"`
so the model can't emit `12.02.2003`-style strings the validator
would reject later. Confidence fields are constrained to
`number, minimum: 0, maximum: 1`.

### Tests added

* `schema_for_metadata_binds_closed_vocabulary_via_enum` — verifies
  each allowlist lands as an `enum` in the right place, plus
  `maxItems` caps and the force-empty `new_tags` array.
* `schema_for_metadata_empty_allowed_list_collapses_to_null_only` —
  pins the empty-list fallback so a future refactor can't silently
  let the sampler invent values.
* `schema_for_metadata_returns_none_when_no_keys_enabled` — short-
  circuits when there's nothing to produce.
* `ollama_chat_payload_forwards_response_schema_to_format_field` and
  `ollama_chat_payload_omits_format_when_schema_unset` — pin the wire
  shape so the schema can't be silently moved elsewhere on the
  Ollama payload.

The `cargo run -p archivist-ai --example dump_metadata_prompt` target
now also dumps the generated JSON Schema so operators can eyeball the
full prompt + schema combination before rolling a change.

### Effect on the prod incident pattern

The `UnknownChoice` cohort (document-label-as-field-name) that
triggered the v1.5.28 and v1.5.29 work is now blocked at the sampler:
the model literally cannot emit `{"name": "Rechnungsnummer"}` if
"Rechnungsnummer" is not in `<allowed_custom_field_names>`, because
that token sequence is masked out of the logits at every position
where `name` is being decoded.

The prompt still describes the constraint in plain English because
(a) the model understands and follows it better, and (b) the
non-Ollama providers don't see the schema yet.

## v1.5.29 — Full prompt redesign for the consolidated metadata extractor

Builds on v1.5.28's narrowly-scoped custom-fields tightening and applies
a deeper redesign grounded in the prompt-engineering literature (Anthropic
prompt-engineering guide, OpenAI structured-outputs guide, Google Gemini
structured-output guide) and a survey of comparable open-source projects
(paperless-gpt, paperless-ai). The contract the worker emits is now
structured for long-context reading instead of dense prose, and every
section is wrapped in named XML tags so the model can disambiguate
instructions from variable inputs.

### What changed in the wire format

**System prompt** is rewritten as a numbered `<rules>` block of eight
single-sentence rules, ordered with anti-hallucination first
(*"It is always better to omit a key, return null, or return [] than
to invent a value"*) and the untrusted-evidence guardrail last. Each
rule addresses one concern; the previous `concat!` of run-on sentences
is gone.

**User prompt** now follows a fixed section order chosen for both
long-context recall and the recency-effect on the final instruction:

```
{language context}
<doc_type_hint>...</doc_type_hint>          # only if non-empty
<requested_keys>...</requested_keys>
<output_schema>{...}</output_schema>
<examples>...</examples>                    # static, 4 examples
<examples key="fields">...</examples>       # only if fields enabled
<allowed_document_types>...</allowed_document_types>
<allowed_correspondents>...</allowed_correspondents>
<allowed_tags>...</allowed_tags>
<allowed_custom_field_names>...</allowed_custom_field_names>
<document>{OCR text, ≤16 000 chars}</document>
Return the single JSON object now. ...
```

The document block sits immediately after the allowlists (so the
closed-vocabulary constraints frame how the model reads it) and the
final "Return the JSON object now" trigger is the last thing on the
wire (recency effect).

**Allowlists** for every closed-vocabulary key (document_type,
correspondent, tags, custom-field names) are now rendered uniformly:
a one-sentence header that names the constraint, then quoted-bullet
entries (`  - "Value"`), wrapped in a matching `<allowed_*>` XML tag.
Empty lists collapse to `(none configured)` with the matching
empty-array / omit-key instruction. Backslashes and quotes inside
values are escaped so an allowed value with embedded punctuation
doesn't break the wire format.

**Output schema** keys are ordered identifying-first
(title, document_date) and classification-last
(document_type, correspondent, tags, fields). JSON property order
acts as a chain-of-thought scaffold: the model reasons about the
identifying fields before committing to a closed-vocabulary class.
`parse_metadata_suggestion` uses unordered key lookups, so the wire
order is a pure prompt-engineering knob with no parsing risk.

**Few-shot examples** are wrapped in `<example>` / `<examples>` tags
per Anthropic's recommendation. A fourth simple-keys example was
added: a partially-OCR'd handwritten note where no allowed
document_type matches, no correspondent is identifiable, and no
explicit date is present — the right output is a title only and
the other keys omitted entirely. This demonstrates the null-fallback
contract from rule 2 of the system prompt by example rather than by
explicit negation.

The fields-specific few-shot pair from v1.5.28 was reformatted to
the same XML structure and is appended only when the fields branch
is enabled.

### Same treatment applied to `prompt_for_fields`

The standalone fields prompt (used when an operator runs the Fields
stage independently of the consolidated metadata stage) received
the same XML-section + numbered-rules treatment. Same wording for
the anti-hallucination directive, same `<allowed_custom_field_names>`
block, same final "Return the JSON object now" trigger. Keeps the
two paths visually identical so an operator reading prompts in the
audit log doesn't have to context-switch between formats.

### Tests added

Three structural tests that pin the design so future refactors can't
silently collapse it:

* `metadata_prompt_uses_xml_section_markup_for_long_context_models` —
  verifies every required XML tag appears and the anti-hallucination
  directive is present verbatim.
* `metadata_prompt_document_block_lives_below_allowlists_and_above_final_trigger` —
  verifies the section order (allowlists → document → trigger,
  schema before document).
* `metadata_prompt_output_schema_lists_identifying_keys_before_classification_keys` —
  verifies title and document_date precede document_type, correspondent,
  tags, fields in the wire schema.

A new `cargo run -p archivist-ai --example dump_metadata_prompt`
target renders the consolidated prompt with a representative
German invoice payload so an operator can eyeball the full wire
format end-to-end without spinning up the worker.

### Not yet (deliberate)

Decoder-side enforcement (Ollama `format: <json-schema>` /
OpenAI `response_format: json_schema` / Anthropic tool-use enums)
remains future work. The literature is unanimous that soft prompt
constraints leave 1–10% slippage even with the best wording; only
constrained decoding closes the gap entirely. Adding it requires
per-provider client changes plus a JSON-schema generator from the
runtime allowlists, which is a bigger surface than fits in this
release.

## v1.5.28 — Tighten the consolidated metadata prompt's custom-fields binding

Follow-up to the v1.5.27 post-mortem. The dashboard surfaced dozens of
`UnknownChoice` validation warnings on the fields branch: the LLM was
pulling document labels (Rechnungsnummer, Kunde, Datum, Police Nr.,
Versicherte(r), …) straight out of the OCR text as `fields[].name`,
even though those labels were not in the configured `Allowed custom
fields` list. A real prod artifact (run on a SCHRACK domain invoice)
returned 18 such entries, all with confidence `0.95`. The validation
correctly rejected them, but every such call wasted a metadata cycle
and the operator saw a noisy review queue.

Three prompt changes, no behaviour change for any other key:

**System prompt now hard-binds `fields[].name`.** Adds two new
sentences (after the closed-vocabulary general rule): one that the
name MUST appear verbatim in the user-prompt's allowed list; one
that names a specific set of common forbidden labels (Rechnungsnummer,
Kunde, Datum, Police Nr., Versicherte(r)) so the model can't
generalise its way out of the constraint. Plus an empty-list
fallback: "If no allowed custom field has clear evidence, return an
empty fields array rather than substituting document labels."

**User-prompt allowed-list block restructured.** Was previously a
newline-separated list under "Allowed custom fields, one per line:".
Now framed as quoted bullets with an explicit "use ONLY these exact
strings, copied verbatim" header and a repeated negative example
("Document labels that look like field names … are NOT acceptable
substitutes"). When the operator hasn't configured any custom
fields, the block collapses to "(none configured)" + a direct
instruction to return `"fields":[]` rather than dangling a confusing
empty list.

**New fields-specific few-shot examples**, sent only when fields is
enabled (so the closed-vocabulary shape doesn't leak into responses
for runs that disabled the fields stage). Example A: allowed list
`["Invoice Number", "Total"]`, document has both "Rechnungsnummer"
and "Gesamtbetrag" as labels — the right answer maps to the allowed
names verbatim, ignoring the label strings. Example B: allowed list
overlaps nothing in the document — the right answer is
`{"fields":[],"confidence":0.95}`, not document labels.

Schema-shape line for `fields` was also sharpened: `"name":"<must be
one of the Allowed custom fields listed above, copied verbatim>"`
plus an inline reminder that an empty list is a valid response.

Four new unit tests pin (a) the system-prompt strict-vocabulary
clause, (b) the user-prompt negative-example block, (c) the quoted
bullet allowed-list formatting, (d) that the fields few-shot only
appears when the fields branch is enabled, and (e) that an empty
allowed list collapses to the safe `(none configured)` instruction.

## v1.5.27 — Quota-aware backoff for Ollama Cloud + operator unblock action

Driven by the 33-hour processing stall on 2026-05-22, when Ollama Cloud's
weekly cap hit and the worker burned 2 298 retries against a 429 that
the provider was never going to clear until the following Monday. Two
related fixes ship together.

### Quota-aware backoff

A 429 response whose body matches a "usage limit" / "quota" / "weekly
limit" / "monthly limit" / "daily limit" signal is now surfaced as a
**typed `AiProviderError::QuotaExhausted`** by every provider client
(Ollama, OpenAI-compatible, Anthropic). The worker classifier maps this
to a new `ProcessingFailureClass::ProviderQuota` which:

* **Is not retryable** — single attempt, the job is marked failed
  immediately rather than burning through the per-job retry budget.
* **Persists a cooldown row** in the new `ai_provider_cooldowns` table.
  Cooldown end is `max(now + Retry-After, now + 24 h)` so a provider's
  optimistic `Retry-After: 30` doesn't shorten the operator's intent.
* **Short-circuits subsequent claims**: at the top of `process_job` the
  worker checks whether the active provider for the stage is in
  cooldown; if so it releases the lease back to the queue with
  `attempts = max(attempts - 1, 0)` and `run_after = cooldown_until`.
  Other jobs whose stage routes to a different (uncooled) provider keep
  flowing — no global pause.

### Operator unblock action on the dashboard

When a permanent failure (quota cohort or otherwise) leaves a run with a
`failed` predecessor stage, `claim_jobs` refuses to hand out the later
queued stage. Before this release the only recovery path was direct SQL.
Two new dashboard alerts surface this state:

* **Blockierte Jobs** (`blocked_jobs`) — count split by failed
  predecessor (critical) vs. waiting_review (warning). Action button
  "Jobs entsperren" calls `POST /api/operations/unblock-jobs`, which
  resets the failed predecessors back to `queued` with `attempts = 0`
  and (by default) drops every active provider cooldown so the next
  claim retries immediately.
* **Provider-Cooldown** (`provider_cooldown`) — lists currently-paused
  providers with reason + expiry. Action button "Cooldown aufheben"
  calls `POST /api/operations/provider-cooldowns/clear`.

Both actions audit-log `operations.jobs_unblocked` and
`ai.provider_cooldown_cleared` so post-hoc you can see who triggered the
recovery.

### Internal surface changes

* `AiProviderError` gains the `QuotaExhausted { provider, message, retry_after }` variant.
* Migration `0025_ai_provider_cooldowns.sql` creates the cooldown table.
* New DB helpers: `upsert_provider_cooldown`, `get_active_provider_cooldown`,
  `list_active_provider_cooldowns`, `clear_provider_cooldown`,
  `clear_all_provider_cooldowns`, `unblock_jobs_from_failed_predecessors`,
  `count_blocked_queued_jobs`.
* New API endpoints: `POST /api/operations/unblock-jobs`,
  `GET /api/operations/provider-cooldowns`,
  `POST /api/operations/provider-cooldowns/clear`.
* New audit event types: `ai.provider_quota_exhausted`,
  `ai.provider_cooldown_cleared`, `operations.jobs_unblocked`.

### Not in this release (deliberate)

* **Prompt-quality issues** that surfaced during the v1.5.26 post-mortem
  — `UnknownChoice` validation warnings on the custom-fields branch of
  the metadata prompt (the model returns field NAMES as VALUES) — are a
  separate prompt-engineering investigation, not bundled here.

## v1.5.26 — Fix worker panic on multi-byte chars at the date-anchor window edge

`document_date_has_anchor()` in `archivist-core` heuristically widens an
80-byte window around each candidate date in the OCR text and checks
that window for phrases like "Rechnungsdatum" before accepting the
date. The window endpoints were computed as raw byte offsets — fine
for ASCII text, but on German OCR (lab reports, invoices) the
saturating subtraction landed inside a multi-byte UTF-8 sequence
(`ä`, `ö`, `ü` are 2 bytes each) and `&text_lower[window_start..window_end]`
panicked:

```
thread 'tokio-rt-worker' panicked at crates/archivist-core/src/lib.rs:2289:
start byte index 649 is not a char boundary; it is inside 'ä' (bytes 648..650)
```

The panic killed the metadata stage for the affected document. Other
in-flight jobs survived, but each affected document required manual
review or a manual retry.

Fix: snap both window endpoints to char boundaries via the existing
`previous_char_boundary` / `next_char_boundary` helpers. Regression
test added that builds an OCR text with 60× `ä` padding so the byte
offset of the date anchor lands deep inside a multi-byte run — the
test panics on `main` before this commit and passes after.

## v1.5.25 — Rebuild v1.5.24 on a real AMD64 runner

No code changes vs. v1.5.24. The `v1.5.23` and `v1.5.24` container images
were produced by a Kaniko build that ran on the `eyeofharmony` runner
pool — the host advertised both `-amd64` and `-arm64` runner tags but
underneath only had an ARM64 Docker daemon, so every build came out
`aarch64` regardless of which tag the job picked. The resulting images
crashed immediately on the AMD64 Talos nodes with
`exec /usr/local/bin/archivist-api: exec format error`, and the rollouts
stalled in `CrashLoopBackOff` before any new traffic was served (old
v1.5.22 pods kept production healthy throughout).

The `kali-docker-amd64` runner is a real x86 host. Tagging `v1.5.25`
forces the deploy pipeline's `release:detect-source` to redo the build
on that runner instead of treating v1.5.24 as already-promoted.

## v1.5.24 — Honest "average processing time" on the operations dashboard

The Operations dashboard's `Durchschn. Bearbeitungszeit` KPI was computed
as `avg(finished_at - started_at)` over `pipeline_runs`. That's the
wall-clock latency of a run, which on a real deployment is dominated by
time the run sat in `waiting_review` (between human clicks), time in
`applying`, and any wall-clock gap when the worker was offline. The KPI
showed values like **`123 h 53 m`** — technically accurate as a "time
the row was open" measure, completely useless as a processing-time
signal.

The query now sums `ai_artifacts.duration_ms` across each run (the
actual AI compute time) and averages across runs that finished in the
window. On prod, that immediately drops the displayed metric from
~124 h to roughly **`37 s` per document** — the number an operator
actually wants when asking "how long does processing take."

No schema change, no migration; the dashboard KPI just stops lying.

## v1.5.23 — Scalable worker pool + DB connection knob

Operational follow-up to the v1.6.2 tuning work. The DB connection pool
was previously hardcoded at 10 and the worker pod shipped with 1 GiB /
2 cores — both became hard ceilings the moment an operator dialed
`ARCHIVIST_WORKER_CONCURRENCY` past a handful, surfacing as
`pool timed out while waiting for an open connection` errors and OOMKill
events under load.

### What changed

* **`ARCHIVIST_DB_MAX_CONNECTIONS`** — new env var (default `10`,
  preserves prior behavior). Should comfortably exceed
  `ARCHIVIST_WORKER_CONCURRENCY`; documented inline in
  `deploy/kubernetes/base/configmap.yaml`. Validated `> 0` at startup.
* **Worker pod resources** bumped in `deploy/kubernetes/base/`: requests
  `250m/512Mi → 500m/1Gi`, limits `2/1Gi → 4/4Gi`. The new headroom is
  what makes high-concurrency OCR survive — rendered PDF pages live in
  memory while jobs run.

### Rollout

Backwards compatible — old deployments keep their 10-connection pool
unless they opt in. To raise concurrency in prod after this image rolls:

```
kubectl -n paperless-archivist set env deploy/paperless-archivist-worker \
  ARCHIVIST_DB_MAX_CONNECTIONS=30 ARCHIVIST_WORKER_CONCURRENCY=8
```

Step up from there once Pod-memory, CPU throttling, and pool
acquisition error counts look clean for an hour.

## v1.5.22 — Per-provider tuning profiles (milestone v1.6.2)

Closes GitLab milestone v1.6.2 (issues #125–#129). Lets an operator
keep a "Local Ollama on a small GPU" configuration and an "OpenAI
cloud API" configuration in the same DB at the same time, switchable
by flipping `ai.default_provider`. The tuning knobs that previously
lived as global env or as scattered `ai.*` / `workflow.*` / `ocr.*` /
`metadata.*` fields move into a per-provider `tuning:` block. The
active default provider's tuning wins; unset fields fall through to
the existing globals (backwards compatible — existing settings rows
load unchanged via serde defaults).

### Data model

`AiProviderSettings` gains `tuning: ProviderTuning`:

* **Performance** — `worker_concurrency`, `consensus_secondary_text_model`,
  `consensus_date_tolerance_days`, `text_num_ctx`, `vision_num_ctx`
* **Resource caps** — `ocr_page_limit`, `hourly_document_limit`,
  `daily_document_limit`
* **Quality thresholds** — `metadata_confidence_threshold`,
  `title_*`, `correspondent_*`, `document_type_*`, `document_date_*`,
  `tags_*`, `fields_*` (each `Option<f32>`), `max_tags`,
  `allowed_list_max`

Every default constructor (`ollama_default`, `openai_default`,
`anthropic_default`, `ollama_cloud_default`,
`openai_compatible_default`) ships sensible presets per the
`docs/PROVIDER_TUNING_PLAN.md` table — Ollama gets `worker_concurrency=2,
text_num_ctx=4096, ocr_page_limit=2, hourly/daily=200/2000`; OpenAI
gets `worker_concurrency=8, consensus_secondary='gpt-4o-mini',
ocr_page_limit=8`; etc.

### Resolution

New `RuntimeSettings::effective_tuning()` returns the fully-resolved
values. For OCR there's also `effective_tuning_for_stage(Stage::Ocr)`
because the OCR-stage provider may differ from the default. 17
call-sites across `archivist-worker`, `archivist-api`, and
`archivist-db` were migrated to read through `effective_tuning()`
instead of the old globals.

### Worker live-reload

`ARCHIVIST_WORKER_CONCURRENCY` becomes a **hard upper cap**. The live
value comes from `effective_tuning.worker_concurrency`. On every
`claim_jobs` cycle the worker recomputes
`target = min(env_cap, settings.worker_concurrency).max(1)`. Pool
upscales by spawning additional tasks; downscale lets surplus tasks
exit *after* their current job — never aborts in-flight work. Audit
event `workflow.concurrency_changed` is emitted on transitions with
`{from, to, env_cap}` so post-hoc you can see when an operator
tuned.

### Ollama runtime hints

New `GET /api/ai/runtime-hints?provider=<name>` (requires
`read_settings`). For Ollama providers it calls `/api/version` and
`/api/ps` and returns the live state: version, loaded models with
VRAM, reachability. `num_parallel` / `max_loaded_models` /
`keep_alive` are explicitly `null` because those values live in the
Ollama pod's env, not on Archivist — the `hint` field carries the
copy-pasteable `kubectl set env deploy/ollama …` example.
Non-Ollama providers return a stub with a kind-appropriate hint.

### Settings UI

Each provider card in Settings → AI grows a collapsible **Tuning**
disclosure with three sub-blocks (Performance / Resource caps /
Quality thresholds), each with a per-block "Reset to defaults"
button that writes the kind-appropriate preset. For Ollama providers
only, a fourth read-only **"Ollama server hints"** panel polls
`/api/ai/runtime-hints` on mount and shows the loaded model + VRAM +
version + the kubectl-edit warning box. 32 new i18n keys × 7 locales
(parity 605 keys / 0 missing).

### How to verify in prod

1. Settings → AI → Ollama provider → open Tuning. Adjust
   `worker_concurrency` from 2 to 1. Save.
2. Within ~5 seconds, watch the worker drop to 1 concurrent job (in
   `/metrics`, `paperless_archivist_jobs_running`). The audit tab
   shows `workflow.concurrency_changed {from: 2, to: 1}`.
3. Reset back to default (button writes 2). Pool grows again.
4. Open the Ollama hints panel — should show the loaded model
   and VRAM matching `kubectl exec -n <ns> deploy/ollama --
   nvidia-smi`.
5. Add an OpenAI provider (or just edit its tuning row), set it as
   `default_provider`. `worker_concurrency` jumps to 8, the
   consensus secondary model becomes `gpt-4o-mini`, throughput caps
   lift.

## v1.5.21 — Metadata-trace diagnostic + test architecture (milestone v1.6.1)

Operator-facing diagnostic: for any document that has been through
the metadata stage, surface what the LLM proposed, what passed
validation, what was applied to Paperless, and — for fields that
didn't land — exactly why. Closes GitLab issues #121 (umbrella),
#122 (backend), #123 (frontend), #124 (docs).

### Endpoint

`GET /api/inventory/{document_id}/metadata-trace` (requires
`read_inventory`). Returns a `MetadataTrace` body containing:

* `current_state` — what `document_inventory` reflects right now
  (title / correspondent / document_type / document_date / tags).
* `latest_run` — most-recent metadata-stage `pipeline_run` for the
  document: model, provider, status, `created_at`, `applied_at` (the
  `audit_events.document.patch_applied` timestamp), and the raw
  `ai_artifacts.normalized_output` payload for power users.
* `per_field_outcomes` — exactly 6 entries in fixed order (`title,
  correspondent, document_type, document_date, tags, fields`). Each
  carries an `outcome` (`applied | review | skipped | dropped |
  rejected`), the value where known, the confidence, a canonical
  machine `reason` string, and any `validation_warnings` from the
  associated review item.

`404` with `{"error": "no metadata run for this document"}` when the
document hasn't been through the stage yet.

### Outcome composition

A pure `compute_field_outcome` function in `archivist-api` runs the
5-branch decision tree from `docs/METADATA_TRACE_CONTRACT.md`:
review-item state takes precedence over audit, audit-applied is
authoritative when no review exists, overwrite-disabled fires when
the current state is set and the metadata setting forbids overwrite,
LLM-omitted yields `dropped`, and the fallthrough is
`entity_not_found`. 15 table-driven tests cover every branch, the
ordering invariants, and the response-body assembly.

### Frontend drawer

New `Stethoscope` icon action on each Inventory row opens a drawer
showing the document's current Paperless state, the run header, and
a 6-card grid (one per metadata field) with color-coded outcome
chips. Each card surfaces value, confidence, translated reason, and
warning chips. A collapsed `<details>` at the bottom holds the raw
`llm_suggestion` JSON. 37 new `inventory.diagnose.*` i18n keys
across all 7 locales (en/de/fr/es/it/nl/pl) — parity check at
573 keys / 0 missing.

### Test architecture anchor

New `docs/TESTING_ARCHITECTURE.md` codifies the four-layer test
model used by the project (unit / `migration_smoke` integration /
OpenAPI ↔ generated-client contract / manual prod E2E), spells out
what each layer explicitly does *not* catch, and uses the
metadata-trace endpoint as the worked example for how a new
pipeline feature plugs into every layer. The "rule of thumb"
section codifies the lessons from the v1.5.x regressions: every new
`sqlx::query` gets a `migration_smoke` call; every wire change
goes through a contract doc; every decision tree extracts to a
pure function.

### How to verify in prod

After Argo rollout:

1. Inventory tab → pick a recently-processed document → click the
   stethoscope icon.
2. Drawer should show the 6 per-field cards with outcome chips.
3. For a document where a field didn't land cleanly, the `reason` +
   warnings on that field's card explain why — feed that back into
   threshold tuning or `allow_new_*` settings.

## v1.5.20 — Metadata completion tag + editable in settings

When the metadata stage settles (auto-apply OR an approved review),
the worker now stamps a dedicated completion tag onto the document in
Paperless. Default: `archivist-metadata`. The OCR completion tag
(`archivist-ocr`) was already in place — the new field plugs the gap
for the consolidated metadata stage which previously had no dedicated
completion tag.

`WorkflowTags` gains a `completion_metadata` field with a serde
default of `"archivist-metadata"`, so settings rows that predate the
upgrade load cleanly. `completion_tag_for_stage(Stage::Metadata)` now
returns this name; the existing `apply_patch_with_workflow_tags`
helper (worker + review-approval path in the API) pulls it through
`paperless.ensure_tag(...)` so the tag is auto-created on first use
if it doesn't already exist in Paperless.

Settings UI gains a new section **"Abschluss-Tags / Completion tags"**
with editable inputs for the OCR and Metadata completion tag names
across all seven locales. Leaving a field empty keeps the default.
Other completion tags (per-field title / correspondent / document
type / fields, plus the final `ai-processed`) remain configurable
via the existing settings JSON; only the two operator-facing values
get UI controls here.

## v1.5.19 — Hotfix: metadata stage failing on dedup query

Production hotfix. Bundle D (v1.5.14, #117) added
`find_metadata_dedup_source` to short-circuit the metadata stage when
the same OCR content hash had already settled. The SQL query in that
helper referenced two columns that do not exist on `ai_artifacts`:

* `aa.paperless_document_id` — that column lives on `pipeline_runs`,
  `jobs`, `review_items`, and `audit_events`, never on `ai_artifacts`.
* `aa.normalized` — the column is named `normalized_output`.

Because the dedup check runs on every non-empty content path of
`process_metadata`, every metadata job in prod failed with `column
aa.paperless_document_id does not exist` from the moment v1.5.14
deployed. The throughput graph spike at 21:00 in the v1.5.18
screenshot was real failures, not real success.

Fix: rewrite the query to join `ai_artifacts` to `pipeline_runs` via
`run_id` (the natural FK) and select `normalized_output`. Added a
regression check to `migration_smoke.rs` that calls
`find_metadata_dedup_source` against the empty fresh-migration DB —
this exercises the SQL parser and column resolution against the real
schema, so any future re-introduction of a non-existent column will
fail in CI rather than in front of operators. `sqlx::query` (which
this code uses) is not schema-checked at compile time; the smoke
test now closes that gap for this query.

## v1.5.18 — Dashboard layout robustness + chart tooltip %

Cosmetic but operationally important. Prod screenshots at intermediate
viewport widths (~1500–1900px after sidebar) showed the dashboard
overflowing horizontally — the "Benötigt Aufmerksamkeit" alert button
and the right-most KPI column were clipped — and at narrower widths
the KPI cards stacked into a tall vertical strip while charts ran
alongside, making the page essentially unreadable.

### Layout

* `.kpi-secondary` / `.kpi-tertiary` now use `repeat(auto-fit,
  minmax(140px, 1fr))` instead of a rigid `repeat(4, …)`. Cards wrap
  to the next row as space disappears instead of clipping.
* `.kpi-grid` collapses to a single column at ≤1200px, where the
  hero card + 4-col side grid no longer fit. Hero becomes a normal
  card in that mode.
* `.operations-strip` switches to auto-fit at ≤1400px so the 4
  service cards reflow before the rigid `minmax(280px, 1.2fr) + 3 ×
  minmax(200px, 1fr)` template overruns.
* `.dashboard-ops-grid` (charts + live panel) collapses one breakpoint
  earlier (1300px) so the live panel drops below the analytics column
  instead of squeezing the throughput chart.
* `.alert-item` is now `minmax(0, 1fr) auto auto` (was `1fr auto
  auto`) so the alert text gets a real min-width of 0 and wraps. At
  ≤720px the action button moves to its own row.
* `.dashboard-heading` is now `flex-wrap: wrap`; long header actions
  drop to a new row instead of pushing the alerts-bar off-screen.
* `.workspace` gets `overflow-x: hidden` as a final guardrail so a
  single mis-measured child can't horizontally scroll the whole page.

### Charts

* Throughput and backlog tooltips now display `success_rate` /
  `completion_rate` with a `%` suffix to match the right-Y axis
  label (prod tooltip showed "Erfolgsquote: 100" — read as "100" not
  "100%").
* Both Y-axes get explicit `width` (56 left, 44 right) so the axis
  labels stop overlapping the plot area at narrow widths.

## v1.5.17 — Dashboard alert kinds: match the actual backend strings

Hotfix for v1.5.16. The "Fehler untersuchen" click handler matched
alert kind `recent_failures`, but the real kind emitted by
`needs_attention_items` in archivist-db is `provider_error` — so the
new banner reported "Unbekannter Alert-Typ: provider_error" instead
of navigating.

* Renamed the `provider_error` branch to match the backend string.
  Click on a "N recent failure(s)" alert now navigates to Inventory
  with `?has_error=true` as intended.
* Added navigation for the other two non-write-action kinds:
  `dry_run_active` and `quota_low` now jump to the Settings tab so
  operators can disable dry-run or raise the limit.
* `App.tsx` cross-tab navigator now always rewrites the query string
  (using `''` when no search is provided) instead of leaving stale
  `?has_error=true` behind when navigating away from Inventory.

## v1.5.16 — Dashboard actions wired + Review Auto-Fix

Operator-facing release driven by two prod issues:

1. Dashboard buttons under "Aktuelle Vorfälle" ("Runs wiederherstellen",
   "Leases neu einreihen", "Fehler untersuchen") had hover tooltips but
   their click handlers were partially silent — failed permissions or
   unknown alert kinds simply returned without feedback. The "Fehler
   untersuchen" button on a `recent_failures` alert had no destination
   at all.
2. With `full_auto` enabled, the review queue still accumulated items
   stuck behind `UnknownChoice` / `UnknownField` / `EmptyOutput`
   warnings produced before the v1.5.7 fix or by all-fields-fallthrough
   edge cases. Operators had to clear them one-by-one.

### 1. Dashboard alerts: real actions and explicit feedback

* `recent_failures` alerts now navigate to the Inventory tab with the
  `has_error=true` filter applied. `Dashboard` accepts a new
  `onNavigate(tab, queryString)` prop, wired in `App.tsx` to push the
  query string into `window.history` before swapping tabs so
  `Inventory` reads it from `window.location.search` on mount.
* `recent_failures` (and the existing `stuck_runs` / `expired_leases`
  recovery actions) now surface explicit feedback via the banner
  instead of silently no-oping: permission denial, missing navigation
  target, and unknown alert kinds each produce a localized message
  (`dashboard.alert.permission_denied`,
  `dashboard.alert.navigate_unavailable`,
  `dashboard.alert.unknown_kind`).
* All seven locales (en/de/fr/es/it/nl/pl) include the new keys —
  parity check passes at 532 keys.

### 2. Review Auto-Fix (Mode A: drop unknowns + apply rest)

New backend endpoints under `/reviews/`:

* `POST /reviews/auto-fix-preview` — dry-runs over the pending queue
  and returns `{total_pending, would_apply, would_reject, sample}` so
  the UI can show a confirmation dialog with the expected outcome.
* `POST /reviews/auto-fix` — bulk-applies the same logic and returns
  `{applied, rejected, errors}`.
* `POST /reviews/{id}/auto-fix` — single-item variant.

The cleaning logic lives in `clean_review_patch_for_auto_fix`. For each
review item it inspects `validation_warnings` and the `suggested_patch`
and:

* if any `UnknownChoice` / `UnknownField` / `EmptyOutput` warning is
  present, drops the `custom_fields` block from the patch (the unknown
  values are what made the item land in review — the rest of the patch
  was already validated);
* drops `null` / empty `document_type`, `correspondent`, `tags`,
  `title`, `created` so they don't overwrite existing Paperless
  metadata with empties;
* if any meaningful field survives, returns `Apply` with the cleaned
  patch; otherwise returns `Reject` so the item is closed out instead
  of looped through review forever.

`auto_fix_apply_one` then runs the standard review pipeline:
`review_decision(edited)` with the cleaned patch → `approved` → 
`apply_review_patch`. Reject path uses `review_decision(rejected)`.
Audit events `review.auto_fix_applied` / `review.auto_fix_rejected`
are emitted per item.

UI surface in the Review tab:

* New toolbar button **"Auto-Fix alle"** runs preview → confirmation
  dialog → bulk endpoint and reloads.
* Each ReviewCard footer gets a third button **"Auto-Fix"** next to
  Approve / Reject for per-item recovery.
* Result summary surfaces through the existing banner channel —
  `applied`/`rejected`/`errors` counts after each run.

Mode B (auto-creating missing Paperless entities) is intentionally out
of scope for v1.5.16 — Mode A clears the existing backlog first; Mode
B is queued for a follow-up where the entity-creation API and operator
preview need their own design pass.

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
