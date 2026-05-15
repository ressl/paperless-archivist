alter table document_inventory
  add column if not exists paperless_modified_at timestamptz;

create index if not exists document_inventory_paperless_modified_idx
  on document_inventory (paperless_modified_at desc);

create table if not exists paperless_sync_state (
  archive_name text primary key,
  last_sync_at timestamptz,
  last_delta_cursor timestamptz,
  last_mode text,
  updated_at timestamptz not null default now()
);
