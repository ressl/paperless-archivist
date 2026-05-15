alter table audit_events
  add column if not exists prev_event_hash text,
  add column if not exists event_hash text;

create index if not exists audit_events_hash_idx
  on audit_events (event_hash)
  where event_hash is not null;

create index if not exists audit_events_created_idx
  on audit_events (created_at desc, id desc);

create index if not exists api_tokens_expiry_idx
  on api_tokens (expires_at)
  where revoked_at is null;

create index if not exists ai_artifacts_created_idx
  on ai_artifacts (created_at desc);
