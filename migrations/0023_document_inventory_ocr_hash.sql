-- v1.5.14 — Bundle D #117: content-hash deduplication.
--
-- When the same document is scanned twice (e.g. operator runs the import
-- pipeline a second time after a power blip, or uploads the same PDF
-- under a different filename), the worker currently runs OCR and
-- metadata extraction from scratch for both copies. Both LLM calls are
-- expensive and the resulting metadata is essentially identical.
--
-- This migration adds an `ocr_content_hash` column that the worker fills
-- with sha256(ocr_full_text) at the end of the OCR stage. The metadata
-- stage then checks for another inventory row with the same hash whose
-- metadata stage has already succeeded; on a hit the worker can copy
-- that document's metadata patch as the base for the current document.
--
-- The hash is stored on `document_inventory` (not as a separate table)
-- because lookup is keyed by it and a unique index isn't required
-- (collisions are vanishingly rare for sha256 and we treat any match
-- as advisory anyway).

alter table document_inventory
  add column if not exists ocr_content_hash text;

create index if not exists document_inventory_ocr_content_hash_idx
  on document_inventory (ocr_content_hash);
