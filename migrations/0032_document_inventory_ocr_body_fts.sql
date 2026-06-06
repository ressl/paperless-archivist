-- #217 — full-text retrieval over the OCR body in chat search.
--
-- Until now only `ocr_content_hash` (sha256 of the OCR text) was stored
-- on `document_inventory`. That is enough for content-hash dedup but not
-- for retrieval: chat candidate selection
-- (`search_document_chat_candidates`) could only match the title, the
-- original file name, and the tags via pg_trgm similarity. A document
-- whose body contains the query terms but whose title does not was never
-- surfaced as a candidate.
--
-- This migration persists the full OCR body locally so it can be indexed
-- for full-text search. This is a deliberate storage tradeoff: the OCR
-- text is duplicated here (it already lives in Paperless) so the chat
-- retrieval ranker has a fast, local FTS signal that does not require a
-- round trip to Paperless just to decide which documents are candidates.
--
--   * `ocr_body`      — nullable; the worker fills it at the end of the
--                       OCR stage with the same (sanitized) text it sends
--                       to Paperless. NULL until a document has been
--                       OCR'd by this build.
--   * `ocr_body_tsv`  — generated/stored tsvector over `ocr_body`. The
--                       `'simple'` configuration is intentional: it does
--                       no language-specific stemming or stop-word
--                       removal, which keeps it correct for the mixed-
--                       language corpus this archivist handles. The
--                       two-argument `to_tsvector('simple', ...)` form is
--                       IMMUTABLE (unlike the single-argument form, which
--                       depends on `default_text_search_config`) and so
--                       is valid in a generated column.
--   * GIN index       — backs the `@@` / `ts_rank` lookups.

alter table document_inventory
  add column if not exists ocr_body text;

alter table document_inventory
  add column if not exists ocr_body_tsv tsvector
    generated always as (to_tsvector('simple', coalesce(ocr_body, ''))) stored;

create index if not exists document_inventory_ocr_body_tsv_idx
  on document_inventory using gin (ocr_body_tsv);
