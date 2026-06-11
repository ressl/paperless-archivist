-- 0039: drop the dead ai_providers table and the six fossil per-field
-- inventory status columns that survived the v1.4.0 stage consolidation. #310
--
-- ai_providers: provider configuration has lived in the settings jsonb
-- (`runtime.ai.providers[]`) since GA; the table was seeded once by 0003 and
-- never read or written again (0 references in crates/, frontend/, openapi/ —
-- re-verified). The two stores had already diverged in production (4 vs. 5
-- providers), so keeping the table around is a second source of truth that is
-- wrong. The settings jsonb stays the single source.
drop table if exists ai_providers;

-- The six per-field status columns (tagging/title/correspondent/document_type/
-- document_date/fields) belonged to the pre-v1.4 per-field stages. Since the
-- consolidated `metadata` stage (0019) no write path ever sets them: live
-- production data shows the literal 'unknown' default on 100% of rows for all
-- six. The document_backlog view (recreated by 0012) computes its next_stage
-- from these columns and stages that 0028 removed, and has 0 code references;
-- document_inventory_status_idx covers only the fossil columns (idx_scan=0).
-- Remaining live status columns: ocr_status, metadata_status,
-- current_run_status (CHECKed in 0043).
drop view if exists document_backlog;
drop index if exists document_inventory_status_idx;

alter table document_inventory
  drop column if exists tagging_status,
  drop column if exists title_status,
  drop column if exists correspondent_status,
  drop column if exists document_type_status,
  drop column if exists document_date_status,
  drop column if exists fields_status;

-- The matching per-field backlog counters on dashboard_snapshots were derived
-- from the dropped columns (count(*) where <fossil> not in (...)), i.e. they
-- have recorded the constant total_documents since v1.4.0. No reader selects
-- them (the snapshot readers use total_documents/complete/failed/
-- waiting_review/running only) and they are NOT NULL without defaults, so the
-- trimmed insert in get_backlog_counts/record_dashboard_snapshot requires
-- dropping them. missing_ocr stays — ocr_status is live.
alter table dashboard_snapshots
  drop column if exists missing_tagging,
  drop column if exists missing_title,
  drop column if exists missing_correspondent,
  drop column if exists missing_document_type,
  drop column if exists missing_document_date,
  drop column if exists missing_fields;
