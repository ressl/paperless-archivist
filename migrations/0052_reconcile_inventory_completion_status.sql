-- Reconcile the inventory snapshot with authoritative Paperless completion
-- tags. Historical jobs, runs, reviews, artifacts, and audit events remain
-- unchanged; only the live inventory projection is ratcheted forward.

update document_inventory
   set ocr_status = case
         when has_ocr_completion_tag or has_full_completion_tag then 'succeeded'
         else ocr_status
       end,
       metadata_status = case
         when has_tagging_completion_tag or has_full_completion_tag then 'succeeded'
         else metadata_status
       end,
       complete = has_full_completion_tag,
       updated_at = now()
 where (
         (has_ocr_completion_tag or has_full_completion_tag)
         and ocr_status <> 'succeeded'
       )
    or (
         (has_tagging_completion_tag or has_full_completion_tag)
         and metadata_status <> 'succeeded'
       )
    or complete is distinct from has_full_completion_tag;
