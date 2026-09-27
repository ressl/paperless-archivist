-- 0056 (#447): inventory filters by correspondent / document type and
-- per-user saved inventory views.
--
-- document_inventory.correspondent_id / document_type_id have been synced
-- from Paperless since 0001 but were never filtered on. The inventory list
-- and export now filter by them (`= any($ids)` / `is null`), so back both
-- with plain btree indexes (the table is small; a partial "is not null" index
-- would not serve the "without correspondent" filter).
create index if not exists document_inventory_correspondent_idx
  on document_inventory (correspondent_id);
create index if not exists document_inventory_document_type_idx
  on document_inventory (document_type_id);

-- Saved views are private per user: a named, canonicalised inventory filter
-- query string (validated by the API against the /api/inventory filter
-- parameters before it is stored). Deleting the user removes their views.
create table if not exists inventory_saved_views (
  id uuid primary key default uuidv7(),
  user_id uuid not null references users(id) on delete cascade,
  name text not null check (char_length(name) between 1 and 80),
  query text not null check (octet_length(query) <= 2000),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

-- One name per user (case-insensitive); also serves the per-user listing.
create unique index if not exists inventory_saved_views_user_name_idx
  on inventory_saved_views (user_id, lower(name));
