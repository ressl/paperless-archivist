create index if not exists document_inventory_current_run_idx
  on document_inventory (current_run_status);

create index if not exists document_inventory_incomplete_idx
  on document_inventory (paperless_document_id)
  where complete = false;

create index if not exists document_inventory_current_tags_gin_idx
  on document_inventory using gin (current_tags);

create index if not exists jobs_created_at_idx
  on jobs (created_at);

create index if not exists jobs_status_updated_at_idx
  on jobs (status, updated_at desc);

create index if not exists ai_artifacts_created_at_idx
  on ai_artifacts (created_at desc);

create index if not exists pipeline_runs_trigger_created_idx
  on pipeline_runs (trigger_tag, created_at desc, paperless_document_id);
