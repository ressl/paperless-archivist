# Inventory Status Reconciliation Design

Status: approved by the operator on 2026-07-18

## Goal

Make the Paperless inventory, dashboard, automatic selector, and Paperless
completion tags describe the same document state. Repair the current production
inconsistencies without deleting job, run, review, or audit history and without
starting SGLang or any GPU workload.

## Evidence

Production currently has 6,004 inventory rows and no queued or running jobs.
Thirty-one documents carry `ai-processed` but retain `unknown` OCR and metadata
statuses. Seven more carry `ai-processed` while their latest manual-review run
is `rejected`; the dashboard maps that terminal outcome to pending. Two
documents have both stages at `succeeded` but lack `ai-processed`. Those two
also preserve the history of an older duplicate-enqueue defect, but that
history is immutable evidence and must not be deleted.

The current sync writes `complete` from the full completion tag but only
ratchets OCR from the OCR-stage tag. It never ratchets metadata from either the
metadata-stage or full completion tag. The selector trusts the full tag and
skips those documents, while the dashboard reads the stale stage columns. A
second sync defect lets the worker overwrite every `paperless_modified_at`
with `NULL`, racing the API sync and forcing a physical rewrite of the entire
inventory.

## Considered approaches

An UI-only masking fix would hide the symptom but leave selection, exports, and
future queries inconsistent. A one-time SQL-only correction would repair the
current rows but allow the next sync to recreate the defect. The selected
approach combines invariant-preserving application logic, a one-time migration,
and targeted Paperless-tag reconciliation.

## Design

Paperless completion tags remain the external source of truth. During every
inventory upsert:

- `ai-processed` implies `complete=true`, `ocr_status=succeeded`, and
  `metadata_status=succeeded`;
- `archivist-ocr` independently ratchets OCR to `succeeded`;
- `archivist-tags` independently ratchets metadata to `succeeded`;
- missing tags never downgrade a stage status written by a worker;
- a missing modified timestamp from a partial sync never erases an existing
  timestamp.

Migration 0052 applies the same ratchets to existing rows. It does not change
`pipeline_runs`, `jobs`, `review_items`, or `audit_events`.

The dashboard treats `rejected` as a resolved terminal stage state and derives
full-tagged documents as complete defensively. The automatic selector also
treats `rejected` as terminal, so a user's rejection is not silently retried.
Explicit rerun remains available.

Completion-tag reconciliation accepts either all enabled stage tags or all
enabled inventory stage statuses as proof that the global tag may be added.
The reconciliation still writes through the Paperless API, records its normal
audit event, honors dry-run and document filters, and never invokes an AI
provider. This repairs documents 4579 and 4872 without GPU access.

Both API and worker sync paths parse the Paperless `modified` timestamp through
one shared helper. The upsert preserves a known timestamp when a caller cannot
provide one, preventing concurrent/partial syncs from nulling the inventory.

## Validation and live rollout

Tests must fail before each behavior change and then prove tag ratchets,
timestamp preservation, terminal `rejected` behavior, dashboard counts, and
status-backed completion-tag reconciliation. Migration smoke tests, workspace
tests, formatting, Clippy, and production builds must pass.

After deployment, migration 0052 normalizes the 38 full-tag conflicts. The
completion-tag endpoint runs first in dry-run mode and then only for the two
verified document IDs. A subsequent Paperless sync restores modified timestamps
and confirms zero dashboard-pending documents and zero active jobs. SGLang stays
scaled to zero throughout.

## Compatibility and rollback

The change adds no API fields and removes no data. Run/review rejection history
remains queryable even when the inventory snapshot becomes completed from the
authoritative full tag. Rolling back the binary leaves the normalized database
values valid; migration 0052 is intentionally forward-only.
