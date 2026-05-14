create table dashboard_snapshots (
  id uuid primary key default uuidv7(),
  captured_at timestamptz not null default now(),
  total_documents bigint not null,
  complete bigint not null,
  missing_ocr bigint not null,
  missing_tagging bigint not null,
  missing_title bigint not null,
  missing_correspondent bigint not null,
  missing_document_type bigint not null,
  missing_fields bigint not null,
  waiting_review bigint not null,
  failed bigint not null,
  running bigint not null,
  never_processed bigint not null
);

create index dashboard_snapshots_captured_at_idx
  on dashboard_snapshots (captured_at desc);
