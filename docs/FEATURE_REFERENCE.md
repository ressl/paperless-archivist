# Feature Reference

This reference maps the major Paperless Archivist capabilities to the UI,
backend behavior, security boundary, and operator notes.

## Paperless Inventory

Archivist syncs Paperless documents through the Paperless REST API. It stores a
local inventory with document title, original filename, current tags, completion
tags, correspondents, document types, document date, custom fields, modified
timestamp, language, and processing state.

Operators use inventory for:

- finding documents that still need OCR or metadata
- triggering single-document jobs
- inspecting debug context for why a document is selected or skipped
- comparing Archivist state with Paperless state

## Model Providers

Archivist routes vision/OCR and text calls through configured model
providers. Default provider records cover `ollama`, `ollama-cloud`,
`openai`, `anthropic`, `openai-compatible`, and a disabled `mineru` preset
for vision-only OCR.

The `mineru` provider kind targets a MinerU API server:

- vision/OCR only; selecting it for a text stage returns a clear
  configuration error instead of a model call
- `base_url` points at the MinerU server without a `/v1` suffix
- health check is `GET /docs` rather than a chat call
- the model is fixed to `mineru`; there is no model list to sync
- no token usage or cost statistics are recorded, since MinerU returns
  none
- the OCR prompt setting has no effect, since MinerU ignores prompt,
  temperature, and context-size parameters

OpenAI and OpenAI-compatible providers (including OpenAI-compatible
servers such as SGLang) add two tuning fields:

- `max_output_tokens`: sent as `max_tokens`. Reasoning tokens count
  toward this cap on most servers, so reasoning/thinking models need a
  generous budget. An empty value keeps the server default.
- `structured_output`: `auto` (strict `json_schema`, unchanged default
  behavior), `json_object` (schema-free JSON mode for servers whose
  grammar backend rejects strict schemas), or `off` (no
  `response_format`, prompt-only). In `auto` mode, a 400 response that
  looks like a rejected schema triggers one automatic retry without
  `response_format`.

Reasoning models served through an OpenAI-compatible endpoint may emit inline
`<think>...</think>` or MiniMax `<mm:think>...</mm:think>` blocks; Archivist
strips them before parsing the response. A response that contains only
reasoning content and no final answer fails with a parser-configuration hint
instead of returning empty text. Worker stages and all API-side text consumers
(Prompt Tester, provider connection test, and Document Chat) resolve the
profile of the provider they actually selected. They share its reasoning
effort, output-token cap, structured-output mode where a schema is present,
text context, and per-request timeout; a prompt model override does not switch
or discard that provider profile.

[ADR-014](ARCHITECTURE_DECISIONS.md#adr-014-sglang-minimax-m3-is-a-selectable-multimodal-openai-compatible-provider)
defines the accepted MiniMax M3 integration contract. The exact target is
`ressl/MiniMax-M3-uncensored-NVFP4` under the existing
`openai_compatible` protocol. Its scope includes text consumers plus optional
rendered-page vision/OCR. The disabled preset exposes the exact model in both
selectors; operators may use its vision default, choose another model, or add
an OCR-stage provider/model override. Selection does not enable the provider,
the OCR workflow, or automatic processing. `thinking_mode` and `<mm:think>`
response support are applied on every selected text or vision M3 request,
while worker stages and API consumers share the selected provider's effective
tuning and timeout. The synthetic image and exact-transcription OCR contracts
are release gates for the pinned runtime. The OCR Prompt Tester itself remains
a text wrapper, so operators validate real image input with the live contract
and a manually reviewed OCR job. Use the
[Settings guide](USER_GUIDE.md#sglang-with-minimax-m3-text-and-visionocr) and
[operations runbook](OPERATIONS.md#sglangminimax-m3-operations).

## OCR Pipeline

The OCR stage downloads the Paperless original through the backend/worker,
renders selected pages, calls the configured vision/OCR model, validates the
result, stores a redacted artifact record, and writes OCR text back through the
Paperless REST API when approved or auto-applied.

Key settings:

- OCR page limit
- minimum existing text length
- renderer
- language hint
- vision model

## Tagging

The tagging stage suggests Paperless business tags. It respects include/exclude
rules, confidence thresholds, allowed/new tag policy, workflow tag protection,
and tag output language.

Archivist never selects trigger, completion, failed, AI-control, or processing
status tags as business tags.

## Standard Paperless Metadata

Archivist can classify standard Paperless fields:

| Field | Paperless target | Notes |
| --- | --- | --- |
| Title | document title/content title | Generated from evidence, not prompt instructions inside the document. |
| Correspondent | `correspondent` ID | Chooses from known Paperless correspondents unless new values are allowed. |
| Document type | `document_type` ID | Chooses from known Paperless document types unless new values are allowed. |
| Issue/document date | `created` date | Normalized to `YYYY-MM-DD`; due dates and scan dates are avoided. |

Overwrite settings protect existing Paperless values unless explicitly enabled.

## Custom Fields

Custom-field extraction uses Paperless custom field definitions and optional
operator mappings:

```text
Field name | enabled | aliases | instructions
```

Disabled fields are excluded from prompts. Field values are validated and
redacted in audit/artifact metadata.

## Language Intelligence

Archivist detects document language from OCR/content and stores a BCP-47-like
language tag with confidence. Prompt builders include language context so
metadata extraction can preserve source-language evidence. Tag output language
is a separate setting for generated business tags.

## Workflow Modes

| Mode | Selection | Review | Apply |
| --- | --- | --- | --- |
| Manual trigger + manual review | User clicks single/batch actions | Required | After approval |
| Autopilot selector + manual review | Worker selects documents | Required | After approval |
| Full autopilot | Worker selects documents | Only on validation fallback | Automatic after validation |

Full autopilot is controlled by validation, safety limits, dry-run, pause, and
fallback-to-review settings.

## Completion And Trigger Tags

After successful stages, Archivist can add completion tags such as OCR complete,
tagging complete, and full processed. Trigger tags are removed after the
corresponding stage succeeds. Completion-tag reconcile can dry-run and repair
documents that have stage tags but miss the full completion tag.

## Review Queue

Reviewers can:

- inspect suggested patches
- edit metadata suggestions
- approve
- reject
- batch approve/reject

Every decision writes audit events. Rejected suggestions do not apply to
Paperless.

## Dashboard And Debugging Light

The dashboard shows 24h by default and supports longer ranges. It includes:

- KPI cards
- backlog trend
- queue state
- stage health
- provider usage
- live selector/LLM/Paperless status
- active runs/jobs
- recent failures
- recovery tools

Use the live panel to see whether the worker is selecting documents, calling
models, waiting for Paperless, retrying, or blocked by review/safety settings.

## Document Chat/RAG

Document Chat retrieves matching Paperless inventory/content candidates, sends
the question through the backend to the configured text model, and stores cited
sources with the assistant answer. The frontend never talks to providers
directly.

## Prompts

The Prompt Workbench shows active and older prompt versions per stage. Editing a
prompt creates a new immutable version. Older versions remain available for
comparison and auditability.

## Security Features

- local login with Argon2id
- OIDC SSO
- server-side sessions
- CSRF protection
- RBAC roles
- scoped API tokens
- encrypted secret references
- audit log and audit integrity checks
- secret redaction in settings, audit, and errors

## Notifications

Webhook notifications can report review backlog, repeated failures, and paused
full autopilot. Payloads are operational summaries and avoid document content,
prompts, provider keys, Paperless tokens, and raw secret values.
