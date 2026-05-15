\set ON_ERROR_STOP on
\timing on

\echo ''
\echo 'Preparing synthetic archive with :'doc_count' documents'

truncate table
  notification_state,
  document_chat_sources,
  document_chat_messages,
  document_chat_sessions,
  ai_artifacts,
  review_items,
  jobs,
  pipeline_runs,
  audit_events,
  dashboard_snapshots,
  document_inventory
cascade;

insert into document_inventory (
  paperless_document_id,
  title,
  original_file_name,
  current_tags,
  current_tag_ids,
  ocr_status,
  tagging_status,
  title_status,
  correspondent_status,
  document_type_status,
  document_date_status,
  fields_status,
  current_run_status,
  needs_review,
  complete,
  document_date,
  detected_language,
  detected_language_confidence,
  paperless_modified_at,
  last_seen_at,
  updated_at
)
select
  gs,
  'Document ' || gs,
  'document-' || gs || '.pdf',
  array['archive', case when gs % 7 = 0 then 'invoice' else 'letter' end],
  array[1, case when gs % 7 = 0 then 2 else 3 end],
  case when gs % 5 = 0 then 'succeeded' else 'unknown' end,
  case when gs % 6 = 0 then 'succeeded' else 'unknown' end,
  case when gs % 9 = 0 then 'succeeded' else 'unknown' end,
  case when gs % 10 = 0 then 'succeeded' else 'unknown' end,
  case when gs % 11 = 0 then 'succeeded' else 'unknown' end,
  case when gs % 12 = 0 then 'succeeded' else 'unknown' end,
  case when gs % 13 = 0 then 'succeeded' else 'unknown' end,
  case
    when gs % 37 = 0 then 'running'
    when gs % 41 = 0 then 'waiting_review'
    else null
  end,
  gs % 41 = 0,
  gs % 97 = 0,
  (current_date - (gs % 365))::text,
  case when gs % 3 = 0 then 'de' when gs % 3 = 1 then 'en' else 'fr' end,
  0.9,
  now() - make_interval(secs => gs % 100000),
  now(),
  now() - make_interval(secs => gs % 100000)
from generate_series(1, :doc_count::integer) as gs;

with inserted_runs as (
  insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages, created_at, updated_at)
  select
    gs,
    case
      when gs % 5 = 0 then 'full_auto'
      when gs % 2 = 0 then 'auto_select_review'
      else 'manual_review'
    end,
    case when gs % 3 = 0 then 'auto-selector' else 'manual-batch' end,
    case
      when gs % 29 = 0 then 'waiting_review'
      when gs % 31 = 0 then 'running'
      when gs % 37 = 0 then 'failed'
      else 'queued'
    end,
    '["ocr","title","document_type","correspondent","document_date","tags","fields"]'::jsonb,
    now() - make_interval(secs => gs % 86400),
    now() - make_interval(secs => gs % 43200)
  from generate_series(1, least(:doc_count::integer, 10000)) as gs
  returning id, paperless_document_id, created_at
)
insert into jobs (run_id, paperless_document_id, stage, status, attempts, payload, error_message, created_at, updated_at, run_after)
select
  r.id,
  r.paperless_document_id,
  stage.stage,
  case
    when r.paperless_document_id % 37 = 0 then 'failed'
    when r.paperless_document_id % 31 = 0 then 'running'
    when r.paperless_document_id % 29 = 0 then 'waiting_review'
    when stage.ordinal <= 2 then 'succeeded'
    else 'queued'
  end,
  case when r.paperless_document_id % 37 = 0 then 3 else 0 end,
  jsonb_build_object('priority', stage.ordinal * 10),
  case when r.paperless_document_id % 37 = 0 then 'synthetic failure' else null end,
  r.created_at,
  now() - make_interval(secs => (r.paperless_document_id * stage.ordinal) % 43200),
  now() - make_interval(secs => (r.paperless_document_id * stage.ordinal) % 300)
from inserted_runs r
cross join lateral (
  values
    (1, 'ocr'),
    (2, 'title'),
    (3, 'document_type'),
    (4, 'correspondent'),
    (5, 'document_date'),
    (6, 'tags'),
    (7, 'fields')
) as stage(ordinal, stage);

insert into ai_artifacts (run_id, job_id, stage, provider, model, input_hash, duration_ms, created_at)
select run_id, id, stage, 'ollama', 'synthetic-model', md5(id::text), 800 + (paperless_document_id % 2000), updated_at
from jobs
where status = 'succeeded'
limit least(:doc_count::integer, 20000);

analyze document_inventory;
analyze pipeline_runs;
analyze jobs;
analyze ai_artifacts;
analyze review_items;
analyze audit_events;

\echo ''
\echo 'Backlog counts'
explain (analyze, buffers)
select
  count(*)::bigint as total_documents,
  count(*) filter (where complete)::bigint as complete,
  count(*) filter (where ocr_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_ocr,
  count(*) filter (where tagging_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_tagging,
  count(*) filter (where needs_review or current_run_status = 'waiting_review')::bigint as waiting_review,
  count(*) filter (where current_run_status in ('queued', 'running', 'applying'))::bigint as running
from document_inventory;

\echo ''
\echo 'Inventory page at offset 0'
explain (analyze, buffers)
select paperless_document_id, title, original_file_name, current_tags, ocr_status, tagging_status, current_run_status, last_seen_at
from document_inventory
order by paperless_document_id desc
limit 100 offset 0;

\echo ''
\echo 'Inventory page at deep offset'
explain (analyze, buffers)
select paperless_document_id, title, original_file_name, current_tags, ocr_status, tagging_status, current_run_status, last_seen_at
from document_inventory
order by paperless_document_id desc
limit 100 offset greatest(:doc_count::integer - 100, 0);

\echo ''
\echo 'Dashboard job activity'
explain (analyze, buffers)
select
  count(*) filter (where created_at >= now() - interval '24 hours')::bigint as jobs_created,
  count(*) filter (where status = 'succeeded' and updated_at >= now() - interval '24 hours')::bigint as jobs_succeeded,
  count(*) filter (where status = 'failed' and updated_at >= now() - interval '24 hours')::bigint as jobs_failed
from jobs;

\echo ''
\echo 'Worker claim query'
begin;
explain (analyze, buffers)
with claimed as (
  select id
    from jobs
   where ((status = 'queued' and run_after <= now())
      or (status = 'running' and lease_until < now()))
     and not exists (
       select 1
         from jobs prev
        where prev.run_id = jobs.run_id
          and prev.priority < jobs.priority
          and prev.status in ('queued', 'running', 'waiting_review', 'failed')
     )
   order by case when error_message is not null and attempts > 0 then 0 else 1 end,
            run_after,
            priority,
            created_at
   for update skip locked
   limit 16
),
updated as (
  update jobs j
     set status = 'running',
         lease_owner = 'benchmark',
         lease_until = now() + make_interval(secs => 300),
         attempts = attempts + 1,
         updated_at = now()
    from claimed
   where j.id = claimed.id
  returning j.id, j.run_id, j.paperless_document_id, j.stage, j.status,
            j.attempts, j.max_attempts, j.payload
)
select count(*) from updated;
rollback;

\echo ''
\echo 'Auto-selector candidate scan'
explain (analyze, buffers)
select paperless_document_id
from document_inventory
where coalesce(current_run_status, '') not in ('queued', 'running', 'waiting_review', 'applying')
  and ('{}'::text[] = '{}' or current_tags && '{}'::text[])
  and not (current_tags && array['archive-skip']::text[])
order by paperless_document_id
limit 500;
