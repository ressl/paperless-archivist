-- 0044: drop the remaining unused/unservable indexes (~4 MB). #315
--
-- Same evidence standard as 0035/0042: idx_scan=0 in production (stats never
-- reset) AND a code-shape proof that no existing query can ever use them.
--
--   * ai_artifacts_normalized_output_gin_idx (0001, ~3.4 MB): jsonb_path_ops
--     GIN only serves containment/path operators (@>, @?, @@). The only
--     queries against normalized_output use ->>/#>> extraction and a single
--     `? 'prompt_experiment_group'` existence test (list_prompt_usage) —
--     jsonb_path_ops cannot serve `?` by construction. If
--     list_prompt_experiments ever needs acceleration, recreate as jsonb_ops.
--   * review_items_suggested_patch_gin_idx (0001, ~340 kB): no containment
--     operator against suggested_patch exists anywhere in crates/.
--   * document_inventory_paperless_modified_idx (0015, ~300 kB): the delta
--     sync filters on the Paperless API side (modified__gt) and upserts by
--     primary key; no query orders or filters by paperless_modified_at, so
--     the index stays dead even with delta sync enabled.

drop index if exists ai_artifacts_normalized_output_gin_idx;
drop index if exists review_items_suggested_patch_gin_idx;
drop index if exists document_inventory_paperless_modified_idx;
