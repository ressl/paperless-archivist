-- v1.5.x — remove the seeded prompts for the legacy per-field pipeline
-- stages. As of v1.4.0 the consolidated `metadata` stage replaced the six
-- per-field stages (title, document_type, correspondent, document_date,
-- tags, fields), and the never-executed `ocr_fix` stage was dropped from
-- the Stage enum entirely. The only stages the worker still runs are `ocr`
-- and `metadata` (plus the `apply` orchestration stage, which has no prompt).
--
-- This deletes every prompt row keyed by a removed stage — both the seeded
-- `default` rows and any operator-customised variants, since the worker and
-- API can no longer parse or dispatch these stage names. Foreign keys that
-- point at prompts (e.g. review_items.prompt_id) are `on delete set null`,
-- so historical rows simply lose their prompt linkage rather than blocking
-- the delete. Idempotent: re-running matches nothing once the rows are gone.

delete from prompts
 where stage in (
   'ocr_fix',
   'tags',
   'tagging',
   'title',
   'correspondent',
   'document_type',
   'document_date',
   'issue_date',
   'fields'
 );
