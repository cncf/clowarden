-- Search for changes using the input parameters provided and returns the
-- results in json format.
create or replace function search_changes(p_input jsonb)
returns table(changes json, total_count bigint) as $$
declare
    v_limit int := coalesce((p_input->>'limit')::int, 10);
    v_offset int := coalesce((p_input->>'offset')::int, 0);
    v_sort_by text := coalesce(p_input->>'sort_by', 'date');
    v_sort_direction text := coalesce(p_input->>'sort_direction', 'desc');
begin
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
            r.completed_at as reconciliation_completed_at,
            r.error as reconciliation_error,
            r.pr_number,
            r.pr_created_by,
            r.pr_merged_by,
            extract(epoch from r.pr_merged_at) as pr_merged_at
        from change c
        join reconciliation r using (reconciliation_id)
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
