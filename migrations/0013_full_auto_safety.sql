update settings
   set value = jsonb_set(value, '{workflow,paused}', 'false'::jsonb, true)
 where key = 'runtime'
   and value #> '{workflow,paused}' is null;

update settings
   set value = jsonb_set(value, '{workflow,dry_run}', 'false'::jsonb, true)
 where key = 'runtime'
   and value #> '{workflow,dry_run}' is null;

update settings
   set value = jsonb_set(value, '{workflow,hourly_document_limit}', 'null'::jsonb, true)
 where key = 'runtime'
   and value #> '{workflow,hourly_document_limit}' is null;

update settings
   set value = jsonb_set(value, '{workflow,daily_document_limit}', 'null'::jsonb, true)
 where key = 'runtime'
   and value #> '{workflow,daily_document_limit}' is null;
