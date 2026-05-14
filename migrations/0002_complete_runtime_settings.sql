update settings
   set value = jsonb_set(
       jsonb_set(
         value,
         '{ai,providers}',
         coalesce(
           value #> '{ai,providers}',
           '[{"name":"ollama","kind":"ollama","base_url":"http://ollama:11434","default_text_model":"qwen3:8b","default_vision_model":"glm-ocr","secret_id":null,"enabled":true}]'::jsonb
         ),
         true
       ),
       '{fields}',
       coalesce(
         value -> 'fields',
         '{"confidence_threshold":0.55,"max_fields":20}'::jsonb
       ),
       true
     )
 where key = 'runtime';

insert into prompts (stage, name, version, content, output_schema, active)
select stage, name, version, content, output_schema, active
from (values
  ('ocr', 'default', 1, 'You are an OCR transcription engine for archived documents. Preserve factual text, dates, numbers, totals, names, addresses, and line breaks. Return transcription text only.', null::jsonb, true),
  ('tags', 'default', 1, 'You classify Paperless-ngx documents. Use only allowed business tags unless explicitly asked for new tags. Return strict JSON only.', '{"type":"object","required":["tags","confidence"]}'::jsonb, true),
  ('title', 'default', 1, 'You generate concise Paperless-ngx document titles. Return strict JSON only.', '{"type":"object","required":["title","confidence"]}'::jsonb, true),
  ('correspondent', 'default', 1, 'You classify a document by existing correspondent. Return strict JSON only.', '{"type":"object","required":["name","confidence"]}'::jsonb, true),
  ('document_type', 'default', 1, 'You classify a document by existing document type. Return strict JSON only.', '{"type":"object","required":["name","confidence"]}'::jsonb, true),
  ('fields', 'default', 1, 'You extract Paperless-ngx custom field values from explicit evidence in the document text. Return strict JSON only.', '{"type":"object","required":["fields","confidence"]}'::jsonb, true)
) as defaults(stage, name, version, content, output_schema, active)
where not exists (
  select 1 from prompts p where p.stage = defaults.stage and p.name = defaults.name
)
on conflict (stage, name, version) do nothing;
