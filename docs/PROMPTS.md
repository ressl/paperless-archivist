# Prompt Pack

Paperless Archivist ships with a default prompt pack for the standard document
pipeline. The prompts are stored in PostgreSQL as versioned prompt records and
can be replaced from the UI without rebuilding the application.

## Included Defaults

| Stage | Output | Purpose |
| --- | --- | --- |
| `ocr` | raw text | Vision OCR with preserved layout, dates, amounts, identifiers, and document language. |
| `ocr_fix` | raw text | Optional OCR post-processing that fixes recognition errors without changing meaning. |
| `tags` | JSON | Selects exact Paperless business tags from the allowed list and excludes workflow tags. |
| `title` | JSON | Creates concise titles from explicit document evidence. |
| `correspondent` | JSON | Selects one exact existing correspondent from the allowed list. |
| `document_type` | JSON | Selects one exact existing document type from the allowed list. |
| `document_date` | JSON | Extracts the Paperless document date from explicit issue/date evidence and normalizes it to `YYYY-MM-DD`. |
| `fields` | JSON | Extracts explicit custom-field values, including normalized dates and monetary values. |

## Design Rules

- Prompts treat document content as untrusted evidence, not instructions.
- Classification stages use exact names from the Paperless metadata cache.
- Document date extraction must prefer issue, invoice, letter, contract,
  statement, or certificate dates and avoid due, scan, upload, delivery, and
  processing dates.
- Workflow, trigger, completion, failed, and AI-control tags are excluded from
  business tag suggestions.
- Structured stages return strict JSON so Rust validation can parse, normalize,
  and reject unsafe or low-confidence output.
- OCR stages return text only because they patch Paperless document content.
- The worker stores the prompt ID, model, provider, input hash, normalized
  output, raw response, and duration in AI artifacts for auditability.

## Language Context

Worker-generated prompts include the detected document language as a BCP-47 tag,
the detection confidence, and the configured tag output language. OCR cleanup
and title prompts preserve source-language content. Tagging prompts must return
existing allowed tags exactly as listed; only `new_tags` use the configured tag
output language. This keeps the existing Paperless taxonomy stable and avoids
silent mass translation.

Prompt changes should update `docs/PROMPT_RELEASE_NOTES.md` and extend the
golden-document or prompt-regression fixtures when behavior changes.

## Upgrade Behavior

Migration `0008_default_prompt_pack.sql` inserts prompt version `2` for every
default stage. It activates the new defaults only when the currently active
prompt for that stage is the built-in seed prompt or when no active prompt
exists. Custom active prompts are preserved and the new defaults remain
available as inactive versions.

## Competitive Review

The default pack was designed after reviewing common patterns from Paperless AI
companion projects:

- paperless-gpt: separate prompts for tags, title, correspondent, document type,
  custom fields, and OCR.
- paperless-ai: restricted tag/correspondent lists, custom system prompts, and
  structured document analysis.
- Paperless-AIssist: modular prompts for OCR, OCR fix, title, correspondent,
  document type, tags, custom fields, and combined classification.

Archivist keeps those useful product patterns but uses its own prompt wording
and stricter backend validation rather than trusting model output directly.
