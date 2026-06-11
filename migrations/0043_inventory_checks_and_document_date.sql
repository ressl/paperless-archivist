-- 0043: type document_inventory.document_date as a real date and add the
-- missing CHECK constraints on the surviving status text columns. #315
--
-- document_date was added as text in 0012 although it only ever carries ISO
-- dates (the Paperless `created` field). Live production: 5996/5996 non-null,
-- 0 violations of ^[0-9]{4}-[0-9]{2}-[0-9]{2}$, and every value round-trips
-- through ::date unchanged — the USING cast is verified safe. The Rust side
-- moves to Option<NaiveDate> in the same release; the JSON wire format stays
-- the identical "YYYY-MM-DD" string.
alter table document_inventory
  alter column document_date type date using document_date::date;

-- pipeline_runs/jobs/review_items have had status CHECKs since 0001 while the
-- document_inventory status mirrors and audit_events.outcome accept any text.
-- Value sets are derived from live DISTINCTs UNIONed with every literal the
-- code can write (the live set alone would brick rarely-taken writers — e.g.
-- audit_events.outcome had only 4 distinct values in production but the code
-- also emits applied/dropped/rejected/review/skipped/validation_failed/
-- partial_failure on paths that simply had not fired within the retention
-- window). Added NOT VALID + validated separately so the existing rows are
-- checked under SHARE UPDATE EXCLUSIVE instead of blocking writers.

-- ocr_status / metadata_status. Live: unknown/succeeded/failed (+ rejected on
-- metadata). Writers add 'queued' (vision-crash requeue) and 'waiting_review'
-- (set_inventory_stage_status_tx); 'skipped'/'not_needed' are part of the
-- recognized completion vocabulary in every reader (stage_needs_work, the
-- missing_ocr backlog counter).
alter table document_inventory
  add constraint document_inventory_ocr_status_check
  check (ocr_status in ('unknown', 'queued', 'waiting_review', 'succeeded', 'failed', 'rejected', 'skipped', 'not_needed'))
  not valid;
alter table document_inventory validate constraint document_inventory_ocr_status_check;

alter table document_inventory
  add constraint document_inventory_metadata_status_check
  check (metadata_status in ('unknown', 'queued', 'waiting_review', 'succeeded', 'failed', 'rejected', 'skipped', 'not_needed'))
  not valid;
alter table document_inventory validate constraint document_inventory_metadata_status_check;

-- current_run_status mirrors pipeline_runs.status (NULL = no run yet), so the
-- legal set is exactly the pipeline_runs status CHECK from 0001/0033. A NULL
-- passes a CHECK by definition — no explicit IS NULL arm needed.
alter table document_inventory
  add constraint document_inventory_current_run_status_check
  check (current_run_status in ('queued', 'running', 'waiting_review', 'applying', 'succeeded', 'rejected', 'failed', 'cancelled'))
  not valid;
alter table document_inventory validate constraint document_inventory_current_run_status_check;

-- audit_events.outcome: live DISTINCTs are success/retry/failed/warning; the
-- remaining values are written by code paths verified by grep over crates/
-- (review approve/edit flows, per-field metadata outcomes, the bulk-apply
-- partial_failure and the prompt-validation endpoint).
alter table audit_events
  add constraint audit_events_outcome_check
  check (outcome in ('success', 'retry', 'failed', 'warning', 'applied', 'dropped', 'rejected', 'review', 'skipped', 'validation_failed', 'partial_failure'))
  not valid;
alter table audit_events validate constraint audit_events_outcome_check;
