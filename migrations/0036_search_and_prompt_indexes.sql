-- #275: indexes for hot read paths that previously seq-scanned.

-- The inventory `q` search filters `title ilike '%…%' or original_file_name
-- ilike '%…%'`. A leading-wildcard ILIKE can use a trigram GIN index; without
-- one every search scanned the full document_inventory table. pg_trgm is
-- installed in 0001. The chat-candidate `similarity()` ranking over the same
-- columns benefits from the same indexes.
create index if not exists document_inventory_title_trgm_idx
  on document_inventory using gin (title gin_trgm_ops);

create index if not exists document_inventory_filename_trgm_idx
  on document_inventory using gin (original_file_name gin_trgm_ops);

-- list_prompt_usage and the per-prompt artifact lookups filter ai_artifacts by
-- prompt_id, which had no supporting index.
create index if not exists ai_artifacts_prompt_id_idx
  on ai_artifacts (prompt_id);
