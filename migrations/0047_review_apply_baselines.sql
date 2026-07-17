-- #344: Review suggestions can wait while operators edit the same document
-- directly in Paperless. Persist a field-scoped creation baseline and make
-- optimistic-concurrency conflicts visible without storing document values in
-- the audit event.

alter table review_items
  add column baseline jsonb not null default '{}'::jsonb,
  add column conflict_fields jsonb not null default '[]'::jsonb,
  add column conflicted_at timestamptz;

alter table review_items
  add constraint review_items_baseline_object_check
    check (jsonb_typeof(baseline) = 'object'),
  add constraint review_items_conflict_fields_array_check
    check (jsonb_typeof(conflict_fields) = 'array');
