-- v1.4.0 — Consolidated metadata stage + age-derived job priority.
--
-- Adds:
--   * document_inventory.metadata_status            — completion tracking for the consolidated
--                                                    Stage::Metadata that replaces the six
--                                                    per-field stages (title/tags/...).
--   * jobs.stage_priority (virtual column)          — derived from payload->>'stage_priority'.
--                                                    The legacy `priority` column previously
--                                                    served BOTH cross-run prioritisation AND
--                                                    within-run stage ordering (10, 20, 30...).
--                                                    Splitting them lets `priority` carry the
--                                                    age-derived value (1_000_000 - doc_id) for
--                                                    cross-run ordering while `stage_priority`
--                                                    keeps the stage-ordering invariant inside a
--                                                    single run.
--
-- The split is backward compatible: existing rows have stage_priority = priority because
-- `payload->>'stage_priority'` is null for them and coalesces to the same payload->>'priority'.
-- In-flight runs queued under v1.3.x continue to drain in the correct stage order.

alter table document_inventory
  add column if not exists metadata_status text not null default 'unknown';

create index if not exists document_inventory_metadata_status_idx
  on document_inventory (metadata_status);

-- Virtual stage_priority column. Falls back to the legacy `priority` so in-flight runs queued
-- with payload = {"priority": N} (no stage_priority key) preserve their original stage ordering.
alter table jobs
  add column if not exists stage_priority integer generated always as (
    coalesce(
      (payload ->> 'stage_priority')::integer,
      (payload ->> 'priority')::integer,
      100
    )
  ) virtual;

create index if not exists jobs_stage_priority_idx on jobs (run_id, stage_priority);
