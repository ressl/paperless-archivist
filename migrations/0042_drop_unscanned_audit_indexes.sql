-- 0042: drop the never-scanned audit_events indexes (~21 MB on the hottest
-- insert table — an audit row is written inside nearly every transaction). #310
--
-- All three show idx_scan=0 in production (stats never reset) AND no query
-- site can use them (methodology of 0035/#273, re-verified against every
-- audit_events query in crates/):
--
--   * audit_events_hash_idx (0014, 10 MB): the hash chain has been anchored
--     by chain_position since 0034 — both the prev-hash lookup and the
--     verification replay order by chain_position and plan on
--     audit_events_chain_position_idx; nothing looks rows up by event_hash.
--   * audit_events_actor_created_idx (0001, ~6 MB): no query filters on
--     actor_type/actor_id.
--   * audit_events_document_created_idx (0001, ~5 MB): no query filters
--     audit_events by paperless_document_id.
--
-- Deliberately KEPT despite idx_scan=0: audit_events_job_id_idx and
-- audit_events_run_id_idx (0018) — the provider-feedback join is planner-
-- reachable through them (EXPLAIN shows BitmapAnd(type_created + job_id) for
-- ≥30-day ranges), and the run-correlation lookup uses run_id_idx.

drop index if exists audit_events_hash_idx;
drop index if exists audit_events_actor_created_idx;
drop index if exists audit_events_document_created_idx;
