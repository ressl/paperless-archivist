alter table document_inventory
  add column if not exists detected_language text,
  add column if not exists detected_language_confidence real,
  add column if not exists detected_language_source text,
  add column if not exists detected_language_updated_at timestamptz;

create index if not exists document_inventory_detected_language_idx
  on document_inventory (detected_language, detected_language_confidence);

update settings
   set value = jsonb_set(value, '{tagging,tag_output_language}', '"de"', true)
 where key = 'runtime'
   and value #>> '{tagging,tag_output_language}' is null;
