-- Speed up metrics_snapshot latency aggregates that filter audit_events by
-- event_type and a recent created_at window (see metrics_snapshot in
-- crates/archivist-db/src/lib.rs). Without this index the p95/sum/count
-- aggregates scan the entire unbounded audit_events table.
create index if not exists idx_audit_events_type_created on audit_events (event_type, created_at);
