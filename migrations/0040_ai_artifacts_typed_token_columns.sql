-- 0040: typed token counters on ai_artifacts. #310
--
-- Until now every usage/statistics aggregate re-parsed token counts out of the
-- response jsonb at query time (a 6-branch CASE copy-pasted across four query
-- sites, plus a per-page lateral fallback), de-toasting most of the table on
-- every dashboard render. Store the counters once, typed, at insert time
-- (insert_ai_artifact extracts them from the provider response) and let the
-- aggregates read plain bigint columns.
--
-- ADD COLUMN ... DEFAULT 0 is metadata-only on PostgreSQL 11+; the backfill
-- below physically rewrites only rows that actually carry token data.
alter table ai_artifacts
  add column if not exists input_tokens bigint not null default 0,
  add column if not exists output_tokens bigint not null default 0;

-- Backfill from the stored responses with the exact semantics of the old
-- query-time extraction (see provider_usage pre-0040):
--   input  = usage.prompt_tokens + usage.input_tokens + prompt_eval_count
--   output = usage.completion_tokens + usage.output_tokens + eval_count
-- summing only plain non-negative integers (pre-v1.11.2 releases redacted the
-- numeric counters to the string "[REDACTED]"; redacted-mode artifacts may
-- carry summary objects in their place), plus the per-page pages[] fallback
-- for pre-#259 OCR artifacts that never got a flattened top-level `usage`
-- (0037 normalizes those, but this expression is self-sufficient either way:
-- the fallback only fires when `response ? 'usage'` is false, so flattened
-- rows are never double counted).
update ai_artifacts a
   set input_tokens = u.input_tokens,
       output_tokens = u.output_tokens
  from (
    select t.id,
           (
             case when t.response #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (t.response #>> '{usage,prompt_tokens}')::bigint else 0 end +
             case when t.response #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (t.response #>> '{usage,input_tokens}')::bigint else 0 end +
             case when t.response ->> 'prompt_eval_count' ~ '^[0-9]+$' then (t.response ->> 'prompt_eval_count')::bigint else 0 end +
             page_usage.input_tokens
           ) as input_tokens,
           (
             case when t.response #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (t.response #>> '{usage,completion_tokens}')::bigint else 0 end +
             case when t.response #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (t.response #>> '{usage,output_tokens}')::bigint else 0 end +
             case when t.response ->> 'eval_count' ~ '^[0-9]+$' then (t.response ->> 'eval_count')::bigint else 0 end +
             page_usage.output_tokens
           ) as output_tokens
      from ai_artifacts t
     cross join lateral (
       select
         coalesce(sum(
           case when page.value #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,prompt_tokens}')::bigint else 0 end +
           case when page.value #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,input_tokens}')::bigint else 0 end +
           case when page.value ->> 'prompt_eval_count' ~ '^[0-9]+$' then (page.value ->> 'prompt_eval_count')::bigint else 0 end
         ), 0)::bigint as input_tokens,
         coalesce(sum(
           case when page.value #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,completion_tokens}')::bigint else 0 end +
           case when page.value #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,output_tokens}')::bigint else 0 end +
           case when page.value ->> 'eval_count' ~ '^[0-9]+$' then (page.value ->> 'eval_count')::bigint else 0 end
         ), 0)::bigint as output_tokens
       from jsonb_array_elements(
         case when jsonb_typeof(t.response -> 'pages') = 'array'
                   and t.response -> 'usage' is null
              then t.response -> 'pages'
              else '[]'::jsonb end
       ) as page(value)
     ) page_usage
  ) u
 where u.id = a.id
   and (u.input_tokens, u.output_tokens) is distinct from (a.input_tokens, a.output_tokens);
