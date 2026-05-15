update settings
   set value = jsonb_set(
     value,
     '{ai,providers}',
     (
       select jsonb_agg(
         case
           when provider ->> 'name' = value #>> '{ai,default_provider}' then
             jsonb_set(
               jsonb_set(
                 provider,
                 '{default_text_model}',
                 to_jsonb(coalesce(nullif(value #>> '{ai,default_text_model}', ''), provider ->> 'default_text_model')),
                 true
               ),
               '{default_vision_model}',
               to_jsonb(coalesce(nullif(value #>> '{ai,default_vision_model}', ''), provider ->> 'default_vision_model')),
               true
             )
           else provider
         end
         order by provider_position
       )
         from jsonb_array_elements(coalesce(value #> '{ai,providers}', '[]'::jsonb))
              with ordinality as providers(provider, provider_position)
     ),
     true
   )
 where key = 'runtime'
   and jsonb_typeof(value #> '{ai,providers}') = 'array';
