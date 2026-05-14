create table document_chat_sessions (
  id uuid primary key default uuidv7(),
  title text not null,
  created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create index document_chat_sessions_created_idx
  on document_chat_sessions (created_at desc);

create index document_chat_sessions_user_created_idx
  on document_chat_sessions (created_by, created_at desc);

create table document_chat_messages (
  id uuid primary key default uuidv7(),
  session_id uuid not null references document_chat_sessions(id) on delete cascade,
  role text not null check (role in ('user', 'assistant', 'system')),
  content text not null,
  provider text,
  model text,
  metadata jsonb,
  created_at timestamptz not null default now()
);

create index document_chat_messages_session_created_idx
  on document_chat_messages (session_id, created_at);

create table document_chat_sources (
  id uuid primary key default uuidv7(),
  message_id uuid not null references document_chat_messages(id) on delete cascade,
  paperless_document_id integer not null,
  title text,
  snippet text not null,
  score double precision not null default 0,
  source_kind text not null,
  created_at timestamptz not null default now()
);

create index document_chat_sources_message_idx
  on document_chat_sources (message_id);

create index document_chat_sources_document_idx
  on document_chat_sources (paperless_document_id, created_at desc);
