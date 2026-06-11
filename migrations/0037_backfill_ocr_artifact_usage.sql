-- 0037: backfill flattened token usage onto pre-v1.12 OCR artifacts. #300
--
-- OCR artifacts store the per-page provider responses under response->'pages';
-- since #259 the worker also flattens the summed per-page token counters into
-- a top-level `usage` block, which is what the usage/statistics queries read.
-- Artifacts written before that fix have no top-level `usage`, so the most
-- token-heavy stage was reported as 0 tokens. Aggregate the nested per-page
-- counters into the same top-level shape the worker writes today.
--
-- Mirrors the worker's sum_vision_usage(): per page, usage.prompt_tokens +
-- usage.input_tokens + prompt_eval_count count as input and
-- usage.completion_tokens + usage.output_tokens + eval_count as output. Only
-- plain integers are summed — pre-v1.11.2 releases redacted numeric counters
-- to the string "[REDACTED]", and redacted-mode artifacts may carry summary
-- objects in their place. Artifacts whose pages hold no token data at all are
-- left untouched, exactly like the worker (it only inserts `usage` when a
-- counter was found).
with page_usage as (
  select a.id,
         sum(
           case when page.value #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,prompt_tokens}')::bigint else 0 end +
           case when page.value #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,input_tokens}')::bigint else 0 end +
           case when page.value ->> 'prompt_eval_count' ~ '^[0-9]+$' then (page.value ->> 'prompt_eval_count')::bigint else 0 end
         ) as input_tokens,
         sum(
           case when page.value #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,completion_tokens}')::bigint else 0 end +
           case when page.value #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,output_tokens}')::bigint else 0 end +
           case when page.value ->> 'eval_count' ~ '^[0-9]+$' then (page.value ->> 'eval_count')::bigint else 0 end
         ) as output_tokens
    from ai_artifacts a
   cross join lateral jsonb_array_elements(a.response -> 'pages') as page(value)
   where a.stage = 'ocr'
     and jsonb_typeof(a.response -> 'pages') = 'array'
     and a.response -> 'usage' is null
   group by a.id
)
update ai_artifacts a
   set response = a.response || jsonb_build_object(
         'usage', jsonb_build_object(
           'prompt_tokens', u.input_tokens,
           'completion_tokens', u.output_tokens
         )
       )
  from page_usage u
 where a.id = u.id
   and (u.input_tokens > 0 or u.output_tokens > 0);
