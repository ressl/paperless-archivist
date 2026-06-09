-- #273: drop duplicate indexes on the two hottest insert tables. Each pair
-- covers the same access path, so the redundant one is pure write
-- amplification on every artifact / audit insert (audit rows are written
-- inside nearly every transaction).

-- ai_artifacts: ai_artifacts_created_idx (0014) and ai_artifacts_created_at_idx
-- (0017) are both btree (created_at desc). Keep the 0017 one.
drop index if exists ai_artifacts_created_idx;

-- audit_events: audit_events_type_created_idx (0001, (event_type, created_at
-- desc)) and idx_audit_events_type_created (0029, (event_type, created_at))
-- are functionally equivalent — a btree serves both scan directions. Keep the
-- original 0001 one and drop the 0029 duplicate.
drop index if exists idx_audit_events_type_created;
