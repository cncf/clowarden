-- Search for changes using the input parameters provided and returns the
-- results in json format.
create or replace function search_changes(p_input jsonb)
returns table(changes json, total_count bigint) as $$
declare
    v_limit int := coalesce((p_input->>'limit')::int, 10);
    v_offset int := coalesce((p_input->>'offset')::int, 0);
    v_sort_by text := coalesce(p_input->>'sort_by', 'date');
    v_sort_direction text := coalesce(p_input->>'sort_direction', 'desc');
    v_organization text := (p_input->>'organization');
    v_service text[];
    v_kind text[];
    v_applied_from timestamptz := (p_input->>'applied_from');
    v_applied_to timestamptz := (p_input->>'applied_to');
    v_pr_number bigint[];
    v_pr_merged_by text[];
    v_tsquery_web tsquery := websearch_to_tsquery(p_input->>'ts_query_web');
    v_tsquery_web_with_prefix_matching tsquery;
begin
    -- Prepare filters
    if p_input ? 'service' and p_input->'service' <> 'null' then
        select array_agg(e::text) into v_service
        from jsonb_array_elements_text(p_input->'service') e;
    end if;
    if p_input ? 'kind' and p_input->'kind' <> 'null' then
        select array_agg(e::text) into v_kind
        from jsonb_array_elements_text(p_input->'kind') e;
    end if;
    if p_input ? 'pr_number' and p_input->'pr_number' <> 'null' then
        select array_agg(e::text) into v_pr_number
        from jsonb_array_elements_text(p_input->'pr_number') e;
    end if;
    if p_input ? 'pr_merged_by' and p_input->'pr_merged_by' <> 'null' then
        select array_agg(e::text) into v_pr_merged_by
        from jsonb_array_elements_text(p_input->'pr_merged_by') e;
    end if;

    -- Prepare v_tsquery_web_with_prefix_matching
    if v_tsquery_web is not null then
        select ts_rewrite(
            v_tsquery_web,
            format('
                select
                    to_tsquery(lexeme),
                    to_tsquery(lexeme || '':*'')
                from unnest(tsvector_to_array(to_tsvector(%L))) as lexeme
                ', p_input->>'ts_query_web'
            )
        ) into v_tsquery_web_with_prefix_matching;
    end if;

    return query
    with filtered_changes as (
        select
            c.change_id,
            c.service,
            c.kind as change_kind,
            c.extra as change_extra,
            extract(epoch from c.applied_at) as change_applied_at,
            c.error as change_error,
            r.reconciliation_id,
            r.organization,
            extract(epoch from r.completed_at) as reconciliation_completed_at,
            r.error as reconciliation_error,
            r.pr_number,
            r.pr_created_by,
            r.pr_merged_by,
            extract(epoch from r.pr_merged_at) as pr_merged_at
        from change c
        join reconciliation r using (reconciliation_id)
        where
            case when v_organization is not null then
            r.organization = v_organization else true end
        and
            case when v_tsquery_web is not null then
                v_tsquery_web_with_prefix_matching @@ c.tsdoc
            else true end
        and
            case when cardinality(v_service) > 0 then
            c.service = any(v_service) else true end
        and
            case when cardinality(v_kind) > 0 then
            c.kind = any(v_kind) else true end
        and
            case when v_applied_from is not null then
            c.applied_at >= v_applied_from else true end
        and
            case when v_applied_to is not null then
            c.applied_at <= v_applied_to else true end
        and
            case when cardinality(v_pr_number) > 0 then
            r.pr_number = any(v_pr_number) else true end
        and
            case when cardinality(v_pr_merged_by) > 0 then
            r.pr_merged_by = any(v_pr_merged_by) else true end
        and
            case when p_input ? 'applied_successfully' and (p_input->>'applied_successfully')::boolean = true then
                c.error is null
            else true end
    )
    select
        (
            select coalesce(json_agg(json_strip_nulls(json_build_object(
                'change_id', change_id,
                'service', service,
                'kind', change_kind,
                'extra', change_extra,
                'applied_at', change_applied_at,
                'error', change_error,
                'reconciliation', json_build_object(
                    'reconciliation_id', reconciliation_id,
                    'organization', organization,
                    'completed_at', reconciliation_completed_at,
                    'error', reconciliation_error,
                    'pr_number', pr_number,
                    'pr_created_by', pr_created_by,
                    'pr_merged_by', pr_merged_by,
                    'pr_merged_at', pr_merged_at
                )
            ))), '[]')
            from (
                select *
                from filtered_changes
                order by
                    (case when v_sort_by = 'date' and v_sort_direction = 'asc' then change_applied_at end) asc,
                    (case when v_sort_by = 'date' and v_sort_direction = 'desc' then change_applied_at end) desc
                limit v_limit
                offset v_offset
            ) fp
        ),
        (
            select count(*) from filtered_changes
        );
end
$$ language plpgsql;
