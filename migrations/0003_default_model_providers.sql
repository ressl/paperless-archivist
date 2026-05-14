insert into ai_providers (
  name,
  kind,
  base_url,
  default_text_model,
  default_vision_model,
  enabled
)
values
  ('ollama', 'ollama', 'http://ollama:11434', 'qwen3:8b', 'qwen2.5vl:7b', true),
  ('openai', 'openai', 'https://api.openai.com/v1', 'gpt-5.5', 'gpt-5.5', true),
  ('anthropic', 'anthropic', 'https://api.anthropic.com/v1', 'claude-sonnet-4-20250514', 'claude-sonnet-4-20250514', true),
  ('openai-compatible', 'openai_compatible', 'http://localhost:8000/v1', 'qwen3:8b', 'qwen2.5vl:7b', false)
on conflict (name) do nothing;

with default_providers as (
  select '[
    {
      "name": "ollama",
      "kind": "ollama",
      "base_url": "http://ollama:11434",
      "default_text_model": "qwen3:8b",
      "default_vision_model": "qwen2.5vl:7b",
      "secret_id": null,
      "enabled": true
    },
    {
      "name": "openai",
      "kind": "openai",
      "base_url": "https://api.openai.com/v1",
      "default_text_model": "gpt-5.5",
      "default_vision_model": "gpt-5.5",
      "secret_id": null,
      "enabled": true
    },
    {
      "name": "anthropic",
      "kind": "anthropic",
      "base_url": "https://api.anthropic.com/v1",
      "default_text_model": "claude-sonnet-4-20250514",
      "default_vision_model": "claude-sonnet-4-20250514",
      "secret_id": null,
      "enabled": true
    },
    {
      "name": "openai-compatible",
      "kind": "openai_compatible",
      "base_url": "http://localhost:8000/v1",
      "default_text_model": "qwen3:8b",
      "default_vision_model": "qwen2.5vl:7b",
      "secret_id": null,
      "enabled": false
    }
  ]'::jsonb as providers
)
update settings
   set value = jsonb_set(
     jsonb_set(
       value,
       '{ai,default_vision_model}',
       case
         when coalesce(value #>> '{ai,default_vision_model}', '') in ('', 'glm-ocr')
           then '"qwen2.5vl:7b"'::jsonb
         else value #> '{ai,default_vision_model}'
       end,
       true
     ),
     '{ai,providers}',
     (
       select jsonb_agg(provider order by sort_order)
       from (
         select
           case
             when default_provider ->> 'name' = 'ollama'
              and coalesce(existing_provider ->> 'default_vision_model', '') in ('', 'glm-ocr')
               then jsonb_set(existing_provider, '{default_vision_model}', '"qwen2.5vl:7b"'::jsonb, true)
             else coalesce(existing_provider, default_provider)
           end as provider,
           default_position as sort_order
         from default_providers,
              jsonb_array_elements(default_providers.providers) with ordinality as defaults(default_provider, default_position)
         left join lateral (
           select existing
             from jsonb_array_elements(coalesce(value #> '{ai,providers}', '[]'::jsonb)) as current(existing)
            where current.existing ->> 'name' = defaults.default_provider ->> 'name'
            limit 1
         ) existing(existing_provider) on true

         union all

         select existing as provider, 1000 + existing_position as sort_order
           from jsonb_array_elements(coalesce(value #> '{ai,providers}', '[]'::jsonb)) with ordinality as current(existing, existing_position),
                default_providers
          where not exists (
            select 1
              from jsonb_array_elements(default_providers.providers) as defaults(default_provider)
             where defaults.default_provider ->> 'name' = current.existing ->> 'name'
          )
       ) merged
     ),
     true
   )
 where key = 'runtime';
