-- 0041: FK prep for the pipeline_runs retention added in this release. #310
--
-- apply_security_retention now prunes terminal pipeline_runs (jobs and
-- ai_artifacts follow via their ON DELETE CASCADE). Two FK rules must change
-- BEFORE the first prune ever runs:
--
--   * review_items.run_id was ON DELETE CASCADE — pruning a months-old run
--     would silently delete its review history. Flip to ON DELETE SET NULL
--     (and drop the NOT NULL) so review items outlive their run, like
--     audit_events already do.
--   * document_inventory.last_run_id had no FK at all (0001 line 155): the
--     complete_job reset and the metadata-backfill nudge correlate on it and
--     would silently no-op on a dangling pointer once runs get pruned. Add
--     the FK with ON DELETE SET NULL. Live production has 0 orphans, so
--     VALIDATE is a pure read.
--
-- Constraints are added NOT VALID first and validated separately: VALIDATE
-- only takes SHARE UPDATE EXCLUSIVE, so concurrent writes keep flowing while
-- the existing rows are checked.

alter table review_items alter column run_id drop not null;
alter table review_items drop constraint if exists review_items_run_id_fkey;
alter table review_items
  add constraint review_items_run_id_fkey
  foreign key (run_id) references pipeline_runs(id) on delete set null
  not valid;
alter table review_items validate constraint review_items_run_id_fkey;

alter table document_inventory drop constraint if exists document_inventory_last_run_id_fkey;
alter table document_inventory
  add constraint document_inventory_last_run_id_fkey
  foreign key (last_run_id) references pipeline_runs(id) on delete set null
  not valid;
alter table document_inventory validate constraint document_inventory_last_run_id_fkey;
