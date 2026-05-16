-- v1.4.3 — Align `workflow.enabled_stages` with the consolidated metadata stage.
--
-- v1.4.0 introduced `Stage::Metadata` to replace the six per-field stages (title, document_type,
-- correspondent, document_date, tags, fields). New installs default to `["ocr", "metadata"]` (see
-- `Stage::all_business_stages`). Existing installations carry the legacy seven-stage array in
-- `settings -> workflow -> enabled_stages`. The auto-selector reads that list to decide which
-- stages to enqueue: with no `metadata` entry, `missing_pipeline_stages_for_inventory` never
-- returns `Stage::Metadata`, so new runs queue only OCR and `document_inventory.metadata_status`
-- stays `unknown` forever — the dashboard's "Metadata" row never lights up.
--
-- This migration rewrites the persisted list once. If `enabled_stages` contains any of the legacy
-- per-field stages, they are removed and `metadata` is appended (deduplicated). `ocr` is
-- preserved. Custom selections that already include `metadata` are left alone. Operators who
-- deliberately reduced the set still keep their subset; only legacy-to-consolidated translation
-- happens here.

update settings
   set value = jsonb_set(
         value,
         '{workflow,enabled_stages}',
         (
           select coalesce(jsonb_agg(distinct stage order by stage), '[]'::jsonb)
             from (
               select case
                        when s.value::text in (
                          '"title"', '"document_type"', '"correspondent"',
                          '"document_date"', '"tags"', '"fields"'
                        ) then to_jsonb('metadata'::text)
                        else s.value
                      end as stage
                 from jsonb_array_elements(value -> 'workflow' -> 'enabled_stages') s
             ) translated
         ),
         false
       ),
       updated_at = now()
 where key = 'runtime'
   and value -> 'workflow' -> 'enabled_stages' is not null
   and exists (
         select 1
           from jsonb_array_elements_text(value -> 'workflow' -> 'enabled_stages') legacy
          where legacy in ('title', 'document_type', 'correspondent', 'document_date', 'tags', 'fields')
       );
