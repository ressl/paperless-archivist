-- Make claim_jobs ordering index-backed without changing its semantics.
--
-- claim_jobs orders queued/reclaimable jobs by:
--   1. retry bias  (failed-with-attempts first)
--   2. priority        — cross-run, age-derived (smaller wins)
--   3. stage_priority  — within-run stage ordering (smaller wins)
--   4. run_after, created_at — FIFO tiebreaker
--
-- `stage_priority` was already converted to a STORED generated column in 0019 so it
-- could be indexed (PostgreSQL 18 rejects indexes on VIRTUAL generated columns).
-- `priority` was left VIRTUAL in 0001 and therefore could not participate in an
-- ordering index. It is derived deterministically from the stored `payload` column
-- (coalesce((payload ->> 'priority')::integer, 100)) — exactly the kind of expression
-- that converts cleanly to STORED — so we convert it identically here, preserving the
-- same computed values for every existing and future row.
--
-- A VIRTUAL generated column cannot be altered in place to STORED; it must be dropped
-- and re-added. No index, view, or constraint depends on `priority` (only claim_jobs'
-- runtime SQL references it by name), so the drop/re-add is safe and preserves the
-- exact claim ordering.
do $$
begin
  if exists (
    select 1
      from pg_attribute
     where attrelid = 'jobs'::regclass
       and attname = 'priority'
       and not attisdropped
       and attgenerated <> 's'
  ) then
    alter table jobs drop column priority;
  end if;
end $$;

alter table jobs
  add column if not exists priority integer generated always as (
    coalesce((payload ->> 'priority')::integer, 100)
  ) stored;

-- Partial composite index matching the claim_jobs ORDER BY (priority, stage_priority,
-- run_after, created_at), scoped to the queued rows the claim query reads.
create index if not exists idx_jobs_claim
  on jobs (priority, stage_priority, run_after, created_at)
  where status = 'queued';
