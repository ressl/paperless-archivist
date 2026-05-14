do $$
begin
  if current_setting('server_version_num')::int < 180000 then
    raise exception 'Paperless Archivist requires PostgreSQL 18 or newer. Current server_version_num=%',
      current_setting('server_version_num');
  end if;
end $$;

create extension if not exists pg_trgm;

create table users (
  id uuid primary key default uuidv7(),
  username text not null unique,
  email text unique,
  password_hash text not null,
  enabled boolean not null default true,
  failed_login_count integer not null default 0,
  last_login_at timestamptz,
  password_changed_at timestamptz not null default now(),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table user_roles (
  user_id uuid not null references users(id) on delete cascade,
  role text not null check (role in ('viewer', 'reviewer', 'operator', 'admin', 'auditor')),
  primary key (user_id, role)
);

create table sessions (
  id uuid primary key default uuidv7(),
  user_id uuid not null references users(id) on delete cascade,
  session_hash text not null unique,
  csrf_secret_hash text not null,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  last_seen_at timestamptz,
  created_at timestamptz not null default now()
);

create index sessions_user_expires_idx on sessions (user_id, expires_at desc);

create table api_tokens (
  id uuid primary key default uuidv7(),
  name text not null,
  token_hash text not null unique,
  scopes text[] not null,
  created_by uuid references users(id) on delete set null,
  expires_at timestamptz,
  revoked_at timestamptz,
  last_used_at timestamptz,
  created_at timestamptz not null default now()
);

create table secret_references (
  id uuid primary key default uuidv7(),
  name text not null unique,
  kind text not null check (kind in ('kubernetes_secret', 'docker_secret', 'env', 'mounted_file', 'encrypted_value')),
  reference jsonb not null,
  created_by uuid references users(id) on delete set null,
  updated_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table settings (
  key text primary key,
  value jsonb not null,
  updated_by uuid references users(id) on delete set null,
  updated_at timestamptz not null default now()
);

create table ai_providers (
  id uuid primary key default uuidv7(),
  name text not null unique,
  kind text not null check (kind in ('ollama', 'openai', 'anthropic', 'openai_compatible')),
  base_url text not null,
  default_text_model text,
  default_vision_model text,
  secret_reference_id uuid references secret_references(id) on delete set null,
  enabled boolean not null default true,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table prompts (
  id uuid primary key default uuidv7(),
  stage text not null,
  name text not null,
  version integer not null,
  content text not null,
  output_schema jsonb,
  active boolean not null default false,
  created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(),
  unique(stage, name, version)
);

create unique index prompts_one_active_per_stage_name_idx
  on prompts (stage, name)
  where active;

create table paperless_tags (
  id integer primary key,
  name text not null,
  slug text,
  color text,
  is_workflow boolean not null default false,
  last_seen_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table paperless_correspondents (
  id integer primary key,
  name text not null,
  last_seen_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table paperless_document_types (
  id integer primary key,
  name text not null,
  last_seen_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table paperless_custom_fields (
  id integer primary key,
  name text not null,
  data_type text,
  last_seen_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table document_inventory (
  paperless_document_id integer primary key,
  title text,
  original_file_name text,
  current_tags text[] not null default '{}',
  current_tag_ids integer[] not null default '{}',
  correspondent_id integer,
  document_type_id integer,
  has_ocr_completion_tag boolean not null default false,
  has_tagging_completion_tag boolean not null default false,
  has_full_completion_tag boolean not null default false,
  ocr_status text not null default 'unknown',
  tagging_status text not null default 'unknown',
  title_status text not null default 'unknown',
  correspondent_status text not null default 'unknown',
  document_type_status text not null default 'unknown',
  fields_status text not null default 'unknown',
  current_run_status text,
  last_seen_at timestamptz not null default now(),
  last_run_id uuid,
  last_error text,
  next_required_stage text,
  needs_review boolean not null default false,
  complete boolean not null default false,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create index document_inventory_status_idx
  on document_inventory (ocr_status, tagging_status, title_status, correspondent_status, document_type_status, fields_status);

create table pipeline_runs (
  id uuid primary key default uuidv7(),
  paperless_document_id integer not null,
  mode text not null check (mode in ('review', 'autopilot')),
  trigger_tag text not null,
  status text not null check (status in ('queued', 'running', 'waiting_review', 'applying', 'succeeded', 'rejected', 'failed', 'cancelled')),
  stages jsonb not null,
  started_at timestamptz,
  finished_at timestamptz,
  error_message text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create unique index pipeline_runs_one_active_per_document_idx
  on pipeline_runs (paperless_document_id)
  where status in ('queued', 'running', 'waiting_review', 'applying');

create index pipeline_runs_document_status_idx on pipeline_runs (paperless_document_id, status, created_at desc);
create index pipeline_runs_status_created_idx on pipeline_runs (status, created_at);

create table jobs (
  id uuid primary key default uuidv7(),
  run_id uuid not null references pipeline_runs(id) on delete cascade,
  paperless_document_id integer not null,
  stage text not null,
  status text not null check (status in ('queued', 'running', 'waiting_review', 'succeeded', 'failed', 'cancelled')),
  attempts integer not null default 0,
  max_attempts integer not null default 3,
  lease_owner text,
  lease_until timestamptz,
  payload jsonb not null default '{}',
  result jsonb,
  error_message text,
  created_at timestamptz not null default now(),
  run_after timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  priority integer generated always as (coalesce((payload ->> 'priority')::integer, 100)) virtual
);

create index jobs_status_run_after_idx on jobs (status, run_after, stage);
create index jobs_document_stage_status_idx on jobs (paperless_document_id, stage, status);
create index jobs_lease_until_idx on jobs (lease_until);

create table ai_artifacts (
  id uuid primary key default uuidv7(),
  run_id uuid not null references pipeline_runs(id) on delete cascade,
  job_id uuid references jobs(id) on delete set null,
  stage text not null,
  provider text not null,
  model text not null,
  prompt_id uuid references prompts(id) on delete set null,
  input_hash text not null,
  request jsonb,
  response jsonb,
  normalized_output jsonb,
  duration_ms integer,
  created_at timestamptz not null default now()
);

create index ai_artifacts_run_stage_idx on ai_artifacts (run_id, stage, created_at desc);
create index ai_artifacts_normalized_output_gin_idx on ai_artifacts using gin (normalized_output jsonb_path_ops);

create table review_items (
  id uuid primary key default uuidv7(),
  run_id uuid not null references pipeline_runs(id) on delete cascade,
  job_id uuid references jobs(id) on delete set null,
  paperless_document_id integer not null,
  stage text not null,
  status text not null check (status in ('pending', 'approved', 'rejected', 'edited', 'applied')),
  suggested_patch jsonb not null,
  edited_patch jsonb,
  validation_warnings jsonb not null default '[]',
  reviewed_by uuid references users(id) on delete set null,
  reviewed_at timestamptz,
  created_at timestamptz not null default now()
);

create index review_items_status_created_idx on review_items (status, created_at);
create index review_items_document_idx on review_items (paperless_document_id, created_at desc);
create index review_items_suggested_patch_gin_idx on review_items using gin (suggested_patch jsonb_path_ops);

create table audit_events (
  id uuid primary key default uuidv7(),
  run_id uuid references pipeline_runs(id) on delete set null,
  job_id uuid references jobs(id) on delete set null,
  paperless_document_id integer,
  event_type text not null,
  actor_type text not null,
  actor_id text,
  source_ip text,
  user_agent text,
  before jsonb,
  after jsonb,
  metadata jsonb,
  outcome text not null default 'success',
  error_message text,
  created_at timestamptz not null default now()
);

create index audit_events_document_created_idx on audit_events (paperless_document_id, created_at desc);
create index audit_events_type_created_idx on audit_events (event_type, created_at desc);
create index audit_events_actor_created_idx on audit_events (actor_type, actor_id, created_at desc);

create view document_backlog as
select
  paperless_document_id,
  title,
  ocr_status,
  tagging_status,
  title_status,
  correspondent_status,
  document_type_status,
  fields_status,
  current_run_status,
  case
    when ocr_status not in ('succeeded', 'skipped', 'not_needed') then 'ocr'
    when tagging_status not in ('succeeded', 'skipped', 'not_needed') then 'tags'
    when title_status not in ('succeeded', 'skipped', 'not_needed') then 'title'
    when correspondent_status not in ('succeeded', 'skipped', 'not_needed') then 'correspondent'
    when document_type_status not in ('succeeded', 'skipped', 'not_needed') then 'document_type'
    when fields_status not in ('succeeded', 'skipped', 'not_needed') then 'fields'
    else null
  end as next_stage
from document_inventory;

insert into settings (key, value)
values ('runtime', '{
  "paperless": {
    "base_url": "http://paperless:8000",
    "public_url": null,
    "token_secret_id": null,
    "timeout_seconds": 30
  },
  "ai": {
    "default_provider": "ollama",
    "ollama_base_url": "http://ollama:11434",
    "default_text_model": "qwen3:8b",
    "default_vision_model": "glm-ocr",
    "stage_models": [],
    "external_provider_warning_acknowledged": false
  },
  "workflow": {
    "mode": "review",
    "tags": {
      "trigger_process": "ai-process",
      "trigger_ocr": "ai-ocr",
      "trigger_tags": "ai-tags",
      "trigger_title": "ai-title",
      "trigger_correspondent": "ai-correspondent",
      "trigger_document_type": "ai-document-type",
      "trigger_fields": "ai-fields",
      "completion_processed": "ai-processed",
      "completion_ocr": "ai-processed-ocr",
      "completion_tagging": "ai-processed-tagging",
      "completion_title": "ai-processed-title",
      "completion_correspondent": "ai-processed-correspondent",
      "completion_document_type": "ai-processed-document-type",
      "completion_fields": "ai-processed-fields",
      "review_needed": "ai-review-needed",
      "failed": "ai-failed",
      "failed_ocr": "ai-failed-ocr",
      "failed_tagging": "ai-failed-tagging"
    },
    "enabled_stages": ["ocr", "title", "document_type", "correspondent", "tags", "fields"],
    "fallback_to_review_on_validation_failure": true
  },
  "ocr": {
    "page_limit": 3,
    "min_chars": 10,
    "renderer": "pdftoppm",
    "language_hint": "deu+eng"
  },
  "tagging": {
    "max_tags": 5,
    "allow_new_tags": false,
    "confidence_threshold": 0.55,
    "old_tag_strategy": "keep_existing"
  }
}'::jsonb)
on conflict (key) do nothing;
