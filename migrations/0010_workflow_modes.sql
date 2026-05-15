update settings
   set value = jsonb_set(value, '{workflow,mode}', '"manual_review"', true)
 where key = 'runtime'
   and value #>> '{workflow,mode}' = 'review';

update settings
   set value = jsonb_set(value, '{workflow,mode}', '"full_auto"', true)
 where key = 'runtime'
   and value #>> '{workflow,mode}' = 'autopilot';

update settings
   set value = jsonb_set(value, '{workflow,tags,completion_ocr}', '"archivist-ocr"', true)
 where key = 'runtime'
   and value #>> '{workflow,tags,completion_ocr}' = 'ai-processed-ocr';

update settings
   set value = jsonb_set(value, '{workflow,tags,completion_tagging}', '"archivist-tags"', true)
 where key = 'runtime'
   and value #>> '{workflow,tags,completion_tagging}' = 'ai-processed-tagging';

alter table pipeline_runs drop constraint if exists pipeline_runs_mode_check;

update pipeline_runs
   set mode = 'manual_review'
 where mode = 'review';

update pipeline_runs
   set mode = 'full_auto'
 where mode = 'autopilot';

alter table pipeline_runs
  add constraint pipeline_runs_mode_check
  check (mode in ('manual_review', 'auto_select_review', 'full_auto'));
