update vote set
    results = jsonb_set(results, '{against_percentage}', to_jsonb((results->>'against')::real / (results->>'allowed_voters')::real * 100))
where results is not null
and results != 'null'::jsonb
and not results ? 'against_percentage';

update vote set
    results = jsonb_set(results, '{pending_voters}', '[]'::jsonb)
where results is not null
and results != 'null'::jsonb
and not results ? 'pending_voters';

---- create above / drop below ----
