-- v1.5.14 — Bundle D #116: per-page OCR text cache.
--
-- Vision-model calls are the single most expensive step in the pipeline
-- (~30-180s/page on local Ollama). When an OCR job is re-tried after a
-- transient failure or when the same document is re-triggered, every
-- page is rendered AND fed back through the vision model from scratch.
-- This cache lets the worker skip the vision call when the same page
-- (identified by sha256 of its rendered PNG bytes) has been OCR'd
-- successfully before.
--
-- Keys: (document_id, page_index, page_hash). page_hash makes the cache
-- robust to re-rendering with different DPI/render settings — only an
-- identical render is considered a hit.

create table if not exists ocr_page_cache (
  paperless_document_id integer not null,
  page_index integer not null,
  page_hash text not null,
  ocr_text text not null,
  provider text,
  model text,
  created_at timestamptz not null default now(),
  primary key (paperless_document_id, page_index, page_hash)
);

create index if not exists ocr_page_cache_created_idx
  on ocr_page_cache (created_at);

-- Cleanup index for the retention job: drop rows older than 90 days.
create index if not exists ocr_page_cache_document_idx
  on ocr_page_cache (paperless_document_id);
