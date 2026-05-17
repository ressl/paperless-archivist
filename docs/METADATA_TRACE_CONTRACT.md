# Metadata Trace — Endpoint Contract

Working contract for the v1.5.21 metadata-trace diagnostic. Backend and
frontend implementations must agree on this shape. If a field needs to
change, update this doc first and link the commit from the PR.

## Endpoint

`GET /api/inventory/{document_id}/metadata-trace`

* **Path param** `document_id`: Paperless document id, integer.
* **Auth**: requires `read_inventory` permission.
* **`200`** with `MetadataTrace` body when the document has at least one
  metadata-stage pipeline run.
* **`404`** when no metadata run exists yet for the document. Body:
  `{ "error": "no metadata run for this document" }`.
* **`403`** on missing permission.

## Response shape (`MetadataTrace`)

```jsonc
{
  "paperless_document_id": 4904,
  "current_state": {
    // What document_inventory currently reflects. Names where possible;
    // ids are an internal detail of Paperless.
    "title": "Invoice 2026-04 Acme.pdf",
    "correspondent": "Acme Corp",        // null when not set
    "document_type": "Invoice",          // null when not set
    "document_date": "2026-04-15",       // null when not set
    "tags": ["archivist-ocr", "archivist-metadata", "invoice"]
  },
  "latest_run": {
    "run_id": "01J….uuid",
    "stage": "metadata",
    "status": "succeeded",               // succeeded | failed | waiting_review | running | pending
    "created_at": "2026-05-17T13:42:11Z",
    "model": "qwen3-paperless:8b",       // null when no ai_artifact yet
    "provider": "ollama",                // null when no ai_artifact yet
    "applied_at": "2026-05-17T13:42:14Z",// timestamp of the document.patch_applied audit row; null if not applied
    "llm_suggestion": {                  // raw `normalized_output` from ai_artifacts; null if no artifact
      "title":         { "title": "Invoice 2026-04 Acme",      "confidence": 0.92 },
      "correspondent": { "name":  "Acme Corp",                 "confidence": 0.85, "evidence": "letterhead" },
      "document_type": { "name":  "Invoice",                   "confidence": 0.79 },
      "document_date": { "date":  "2026-04-15",                "confidence": 0.93, "evidence": "issued: 2026-04-15", "warnings": [] },
      "tags":          { "tags":  ["invoice", "tax"],          "confidence": 0.71 },
      "fields":        { "fields": [{ "name": "amount", "value": "1234.50" }], "confidence": 0.83 }
    },
    "per_field_outcomes": [
      // One entry per field the metadata stage is allowed to set,
      // regardless of whether the LLM proposed it. Outcome semantics:
      //   applied   — passed validation and was written to Paperless
      //   review    — a review_item was created (LLM proposed but failed
      //                validation; warnings explain why)
      //   skipped   — pre-existing value + overwrite disabled
      //   dropped   — LLM omitted the field entirely
      //   rejected  — review_item was rejected by an operator
      {
        "field": "title",
        "outcome": "applied",
        "value": "Invoice 2026-04 Acme",
        "confidence": 0.92,
        "reason": null,
        "warnings": []
      },
      {
        "field": "document_date",
        "outcome": "review",
        "value": "2026-04-15",
        "confidence": 0.62,
        "reason": "below_threshold",
        "warnings": [{ "kind": "LowConfidence", "threshold": 0.90, "got": 0.62 }]
      },
      {
        "field": "tags",
        "outcome": "skipped",
        "value": null,
        "confidence": null,
        "reason": "no_proposal",
        "warnings": []
      }
      // …one entry for each of: title, correspondent, document_type,
      // document_date, tags, fields
    ]
  }
}
```

### Field invariants

* `per_field_outcomes` always contains exactly 6 entries in the order
  `title, correspondent, document_type, document_date, tags, fields`.
  Frontends can therefore render them as a fixed grid.
* `outcome` is the canonical word used in i18n keys
  (`inventory.diagnose.outcome.applied`, etc.).
* `reason` is a short machine string when set: `below_threshold`,
  `unknown_choice`, `no_proposal`, `overwrite_disabled`,
  `entity_not_found`, `parse_failure`, `anchor_missing`, `over_max_tags`,
  `rejected_by_operator`. UI translates these via i18n keys; backend
  must use the exact strings listed here so the UI mapping stays
  exhaustive.
* `value` is the field's value when known; `null` otherwise. Type per
  field: title=string, correspondent/document_type=string (name),
  document_date=string (ISO date), tags=`string[]`, fields=array of
  `{name, value}`.

## Data sources

| Field path                       | Source                                                                     |
| -------------------------------- | -------------------------------------------------------------------------- |
| `current_state.*`                | `document_inventory` row by `paperless_document_id`                        |
| `latest_run.*` (header fields)   | most-recent `pipeline_runs` row joined to its `jobs` row where `stage='metadata'` |
| `latest_run.model/provider`      | `ai_artifacts` row matching `run_id` and `stage='metadata'`               |
| `latest_run.llm_suggestion`      | `ai_artifacts.normalized_output` (same row)                                |
| `latest_run.applied_at`          | `audit_events.created_at` where `event_type='document.patch_applied'` and `run_id` matches |
| `per_field_outcomes[…].warnings` | `review_items.validation_warnings` filtered by `run_id` and standard_metadata.field |
| `per_field_outcomes[…].outcome`  | composition logic below                                                    |

### Outcome composition rules (backend)

For each of the 6 fields, in order:

1. If a `review_item` exists for this `run_id` with
   `standard_metadata.field == <field>`:
   - status `pending` → `review`
   - status `approved` → `applied` (with the approved value)
   - status `rejected` → `rejected`
2. Else if an `audit_events.document.patch_applied` row exists for this
   `run_id` and its `after` payload contains the field → `applied`.
3. Else if `current_state.<field>` is set AND the metadata setting
   `overwrite_existing_<field>` is `false` → `skipped`,
   reason=`overwrite_disabled`.
4. Else if `llm_suggestion.<field>` is missing/null → `dropped`,
   reason=`no_proposal`.
5. Otherwise → `skipped`, reason=`entity_not_found` (LLM proposed but
   the named entity didn't exist in Paperless and `allow_new_*` was
   off).

Tests in the backend issue assert each branch of this decision tree.

## SQL helpers (new in `archivist-db/src/lib.rs`)

```rust
pub async fn latest_metadata_run_for_document(
    pool: &DbPool, paperless_document_id: i32,
) -> Result<Option<MetadataRunHeader>>;

pub async fn latest_metadata_artifact_for_run(
    pool: &DbPool, run_id: Uuid,
) -> Result<Option<MetadataArtifact>>;

pub async fn metadata_review_items_for_run(
    pool: &DbPool, run_id: Uuid,
) -> Result<Vec<MetadataReviewItem>>;

pub async fn latest_apply_audit_for_run(
    pool: &DbPool, run_id: Uuid,
) -> Result<Option<MetadataApplyAudit>>;
```

Each gets a regression call in `tests/migration_smoke.rs` that runs it
against the empty fresh-migration DB with throwaway ids and asserts
`Ok(None)` / `Ok(vec![])`. Same pattern as the v1.5.19 regression for
`find_metadata_dedup_source`. Catches "non-existent column" bugs at
CI time instead of in prod.

## Frontend drawer

* Live in `frontend/src/inventory/Inventory.tsx` as a drawer/dialog
  opened by a new per-row "Diagnose" button (icon: `Stethoscope` from
  lucide-react).
* Layout:
  - Header: document id + "current state" name fields.
  - Run header: model, provider, run_id, status badge, applied_at
    relative time, created_at.
  - Per-field grid: 6 cards (title, correspondent, document_type,
    document_date, tags, fields). Each card shows outcome chip
    (color-coded), proposed value, confidence, reason/warnings.
  - Footer: a collapsed `<details>` with the raw `llm_suggestion` JSON
    for power users.
* Outcome chip colors (Tailwind-ish equivalents via CSS):
  - `applied`   → green (success)
  - `review`    → amber (warning)
  - `skipped`   → muted grey
  - `dropped`   → muted grey
  - `rejected`  → red (danger)
* i18n keys: `inventory.diagnose.*` covering button label, drawer
  title, per-field labels, outcome labels, reason labels.

## Permissions

`read_inventory` only — the endpoint is read-only and contains no
write surface. No new permission is introduced.
