-- #342: durable outbox-style state for Paperless document PATCH operations.
-- A request is prepared before HTTP, marked in-flight immediately before the
-- side effect, then confirmed by the response or reconciled by reading the
-- current Paperless document. The stable source-key/hash pair makes retries
-- return the original attempt instead of emitting a duplicate PATCH.

create table paperless_apply_intents (
  attempt_id uuid primary key default uuidv7(),
  source text not null,
  source_key text not null,
  owner_type text not null check (owner_type in ('user', 'worker')),
  owner_id text not null,
  paperless_document_id integer not null,
  run_id uuid references pipeline_runs(id) on delete set null,
  job_id uuid references jobs(id) on delete set null,
  review_id uuid references review_items(id) on delete set null,
  patch_hash text not null,
  patch jsonb not null,
  before_state jsonb,
  response_state jsonb,
  metadata jsonb not null default '{}'::jsonb,
  review_revert_status text check (review_revert_status in ('pending', 'approved', 'edited')),
  state text not null default 'prepared'
    check (state in ('prepared', 'in_flight', 'confirmed', 'reconciled', 'failed', 'finalized')),
  last_error text,
  request_started_at timestamptz,
  confirmed_at timestamptz,
  finalized_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (source_key, patch_hash)
);

create index paperless_apply_intents_recovery_idx
  on paperless_apply_intents (state, updated_at)
  where state in ('prepared', 'in_flight', 'confirmed', 'reconciled');

create index paperless_apply_intents_review_idx
  on paperless_apply_intents (review_id, created_at desc)
  where review_id is not null;

create index paperless_apply_intents_job_idx
  on paperless_apply_intents (job_id, created_at desc)
  where job_id is not null;
