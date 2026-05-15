create table if not exists notification_state (
  event_key text primary key,
  last_sent_at timestamptz not null,
  updated_at timestamptz not null default now()
);
