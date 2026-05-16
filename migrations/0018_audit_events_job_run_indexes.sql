-- Speeds up dashboard `provider_usage` left-join (`audit_events feedback on
-- feedback.job_id = ai.job_id`) and run-correlation queries. Partial because
-- the vast majority of audit_events carry neither column.
create index if not exists audit_events_job_id_idx
  on audit_events (job_id)
  where job_id is not null;

create index if not exists audit_events_run_id_idx
  on audit_events (run_id)
  where run_id is not null;
