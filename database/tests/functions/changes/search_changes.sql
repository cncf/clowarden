-- Tests searching changes, including filters, sorting, paging and JSON output.

-- ============================================================================
-- SETUP
-- ============================================================================

begin;
select plan(47);

-- Returns the last two digits of the ids of the changes found, in result order
create function pg_temp.found_changes(p_input jsonb)
returns text[] as $$
    select coalesce(
        array_agg(right(e.change->>'change_id', 2) order by e.position),
        '{}'
    )
    from search_changes('{"limit": 100}'::jsonb || p_input) s
    cross join lateral json_array_elements(s.changes)
        with ordinality as e(change, position);
$$ language sql;

-- ============================================================================
-- VARIABLES
-- ============================================================================

-- Every change id ends in the chronological position of the change
\set allChangesAsc '{01,02,03,04,05,06,07,08,09,10,11}'
\set allChangesDesc '{11,10,09,08,07,06,05,04,03,02,01}'
\set change01ID '00000000-0000-0000-0000-000000000001'
\set change02ID '00000000-0000-0000-0000-000000000002'
\set change03ID '00000000-0000-0000-0000-000000000003'
\set change04ID '00000000-0000-0000-0000-000000000004'
\set change05ID '00000000-0000-0000-0000-000000000005'
\set change06ID '00000000-0000-0000-0000-000000000006'
\set change07ID '00000000-0000-0000-0000-000000000007'
\set change08ID '00000000-0000-0000-0000-000000000008'
\set change09ID '00000000-0000-0000-0000-000000000009'
\set change10ID '00000000-0000-0000-0000-000000000010'
\set change11ID '00000000-0000-0000-0000-000000000011'
\set failedReconciliationID '00000000-0000-0000-0001-000000000001'
\set mergedReconciliationID '00000000-0000-0000-0001-000000000002'
\set org2ReconciliationID '00000000-0000-0000-0001-000000000003'
\set scheduledReconciliationID '00000000-0000-0000-0001-000000000004'

-- ============================================================================
-- SEED DATA
-- ============================================================================

-- Reconciliation of a pull request that failed in org1
insert into reconciliation (
    reconciliation_id,
    completed_at,
    organization,
    error,
    pr_created_by,
    pr_merged_at,
    pr_merged_by,
    pr_number
) values (
    :'failedReconciliationID',
    '2024-01-05 10:00:00+00',
    'org1',
    'github: something went wrong',
    'carol',
    '2024-01-04 10:00:00+00',
    'dave',
    102
);

-- Reconciliation of a pull request applied cleanly in org1
insert into reconciliation (
    reconciliation_id,
    completed_at,
    organization,
    pr_created_by,
    pr_merged_at,
    pr_merged_by,
    pr_number
) values (
    :'mergedReconciliationID',
    '2024-01-03 10:00:00.654321+00',
    'org1',
    'alice',
    '2024-01-01 09:00:00.111111+00',
    'bob',
    101
);

-- Reconciliation of a pull request in org2
insert into reconciliation (
    reconciliation_id,
    completed_at,
    organization,
    pr_created_by,
    pr_merged_at,
    pr_merged_by,
    pr_number
) values (
    :'org2ReconciliationID',
    '2024-01-11 10:00:00+00',
    'org2',
    'erin',
    '2024-01-09 09:00:00+00',
    'frank',
    201
);

-- Scheduled reconciliation in org1, which has no pull request
insert into reconciliation (
    reconciliation_id,
    completed_at,
    organization
) values (
    :'scheduledReconciliationID',
    '2024-01-08 10:00:00.5+00',
    'org1'
);

-- Changes are inserted out of chronological order so that sorting is required

-- Scheduled gitlab team added in org1
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    extra
) values (
    :'change07ID',
    '2024-01-07 10:00:00+00',
    'team-added',
    :'scheduledReconciliationID',
    'gitlab',
    to_tsvector('team added team2'),
    '{"team": {"name": "team2"}}'
);

-- Team added to the repository in the merged pull request
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    extra
) values (
    :'change02ID',
    '2024-01-02 10:00:00+00',
    'repository-team-added',
    :'mergedReconciliationID',
    'github',
    to_tsvector('101 alice bob repository team added repo1 team1'),
    '{"repo_name": "repo1", "role": "write", "team_name": "team1"}'
);

-- User removed by the org2 pull request
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    extra
) values (
    :'change11ID',
    '2024-01-11 10:00:00+00',
    'user-removed',
    :'org2ReconciliationID',
    'gitlab',
    to_tsvector('201 erin frank user removed heidi'),
    '{"full_name": "heidi"}'
);

-- Team added by the failed pull request, which applied correctly
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    extra
) values (
    :'change05ID',
    '2024-01-05 10:00:00+00',
    'team-added',
    :'failedReconciliationID',
    'gitlab',
    to_tsvector('102 carol dave team added team1'),
    '{"team": {"name": "team1"}}'
);

-- Repository added by the merged pull request, with a null nested in extra
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    extra
) values (
    :'change01ID',
    '2024-01-01 10:00:00.123456+00',
    'repository-added',
    :'mergedReconciliationID',
    'github',
    to_tsvector('101 alice bob repository added repo1'),
    '{"repo": {"name": "repo1", "visibility": null}}'
);

-- Team added by the org2 pull request
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    extra
) values (
    :'change09ID',
    '2024-01-09 10:00:00+00',
    'team-added',
    :'org2ReconciliationID',
    'github',
    to_tsvector('201 erin frank team added team3'),
    '{"team": {"name": "team3"}}'
);

-- Change that failed to apply in the failed pull request
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    error,
    extra
) values (
    :'change04ID',
    '2024-01-04 10:00:00+00',
    'repository-team-added',
    :'failedReconciliationID',
    'github',
    to_tsvector('102 carol dave repository team added repo2 team2'),
    'team not found',
    '{"repo_name": "repo2", "role": "read", "team_name": "team2"}'
);

-- Change that failed to apply in the org2 pull request, on a team whose name
-- starts with the name of another hyphenated team
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    error,
    extra
) values (
    :'change10ID',
    '2024-01-10 10:00:00+00',
    'team-removed',
    :'org2ReconciliationID',
    'slack',
    to_tsvector('201 erin frank team removed sig-cluster-admins'),
    'team not found',
    '{"team_name": "sig-cluster-admins"}'
);

-- Member added to a hyphenated team by the merged pull request
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    extra
) values (
    :'change03ID',
    '2024-01-03 10:00:00+00',
    'team-member-added',
    :'mergedReconciliationID',
    'github',
    to_tsvector('101 alice bob team member added sig-cluster grace'),
    '{"team_name": "sig-cluster", "user_name": "grace"}'
);

-- Scheduled user added in org1, whose name contains a quote
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc,
    extra
) values (
    :'change08ID',
    '2024-01-08 10:00:00+00',
    'user-added',
    :'scheduledReconciliationID',
    'slack',
    to_tsvector('user added O''Brien'),
    '{"full_name": "O''Brien"}'
);

-- Scheduled team removed in org1, without extra details
insert into change (
    change_id,
    applied_at,
    kind,
    reconciliation_id,
    service,
    tsdoc
) values (
    :'change06ID',
    '2024-01-06 10:00:00.000001+00',
    'team-removed',
    :'scheduledReconciliationID',
    'github',
    to_tsvector('team removed team1')
);

-- ============================================================================
-- TESTS
-- ============================================================================

-- Should apply the default limit when none is provided
select results_eq(
    $$
        select json_array_length(changes), total_count
        from search_changes('{}')
    $$,
    $$ values (10, 11::bigint) $$,
    'Should apply the default limit when none is provided'
);

-- Should combine filters with AND
select is(
    pg_temp.found_changes('{
        "kind": ["team-added"],
        "organization": "org1",
        "service": ["gitlab"],
        "ts_query_web": "team2"
    }'),
    array['07'],
    'Should combine filters with AND'
);

-- Should filter by a single kind
select is(
    pg_temp.found_changes('{"kind": ["team-added"]}'),
    array['09', '07', '05'],
    'Should filter by a single kind'
);

-- Should filter by a single pull request merger
select is(
    pg_temp.found_changes('{"pr_merged_by": ["bob"]}'),
    array['03', '02', '01'],
    'Should filter by a single pull request merger'
);

-- Should filter by a single pull request number
select is(
    pg_temp.found_changes('{"pr_number": [102]}'),
    array['05', '04'],
    'Should filter by a single pull request number'
);

-- Should filter by a single service
select is(
    pg_temp.found_changes('{"service": ["slack"]}'),
    array['10', '08'],
    'Should filter by a single service'
);

-- Should filter by organization
select is(
    pg_temp.found_changes('{"organization": "org2"}'),
    array['11', '10', '09'],
    'Should filter by organization'
);

-- Should filter by several kinds
select is(
    pg_temp.found_changes('{"kind": ["team-added", "team-removed"]}'),
    array['10', '09', '07', '06', '05'],
    'Should filter by several kinds'
);

-- Should filter by several pull request mergers
select is(
    pg_temp.found_changes('{"pr_merged_by": ["bob", "dave"]}'),
    array['05', '04', '03', '02', '01'],
    'Should filter by several pull request mergers'
);

-- Should filter by several pull request numbers
select is(
    pg_temp.found_changes('{"pr_number": [102, 201]}'),
    array['11', '10', '09', '05', '04'],
    'Should filter by several pull request numbers'
);

-- Should filter by several services
select is(
    pg_temp.found_changes('{"service": ["gitlab", "slack"]}'),
    array['11', '10', '08', '07', '05'],
    'Should filter by several services'
);

-- Should find hyphenated names by a later word
select is(
    pg_temp.found_changes('{"ts_query_web": "cluster"}'),
    array['10', '03'],
    'Should find hyphenated names by a later word'
);

-- Should find hyphenated names by prefix
select is(
    pg_temp.found_changes('{"ts_query_web": "sig-clu"}'),
    array['10', '03'],
    'Should find hyphenated names by prefix'
);

-- Should find longer names starting with a full hyphenated name
select is(
    pg_temp.found_changes('{"ts_query_web": "sig-cluster"}'),
    array['10', '03'],
    'Should find longer names starting with a full hyphenated name'
);

-- Should find names containing a quote
select is(
    pg_temp.found_changes($${"ts_query_web": "O'Brien"}$$),
    array['08'],
    'Should find names containing a quote'
);

-- Should find pull request numbers
select is(
    pg_temp.found_changes('{"ts_query_web": "101"}'),
    array['03', '02', '01'],
    'Should find pull request numbers'
);

-- Should find terms by prefix
select is(
    pg_temp.found_changes('{"ts_query_web": "ali"}'),
    array['03', '02', '01'],
    'Should find terms by prefix'
);

-- Should hide failed changes when applied_successfully is true
select is(
    pg_temp.found_changes('{"applied_successfully": true}'),
    array['11', '09', '08', '07', '06', '05', '03', '02', '01'],
    'Should hide failed changes when applied_successfully is true'
);

-- Should ignore blank search text
select is(
    pg_temp.found_changes(jsonb_build_object('ts_query_web', v)),
    :'allChangesDesc'::text[],
    format('Should ignore blank search text: %s', label)
)
from (
    values
        ('empty', ''),
        ('mixed whitespace', E' \t\r\n'),
        ('spaces', '   ')
) as blank(label, v);

-- Should ignore empty filter arrays
select is(
    pg_temp.found_changes('{
        "kind": [],
        "pr_merged_by": [],
        "pr_number": [],
        "service": []
    }'),
    :'allChangesDesc'::text[],
    'Should ignore empty filter arrays'
);

-- Should ignore filters set to null
select is(
    pg_temp.found_changes('{
        "applied_from": null,
        "applied_successfully": null,
        "applied_to": null,
        "kind": null,
        "organization": null,
        "pr_merged_by": null,
        "pr_number": null,
        "service": null,
        "sort_by": null,
        "sort_direction": null,
        "ts_query_web": null
    }'),
    :'allChangesDesc'::text[],
    'Should ignore filters set to null'
);

-- Should include applied_from at the exact boundary
select is(
    pg_temp.found_changes('{"applied_from": "2024-01-09 10:00:00+00"}'),
    array['11', '10', '09'],
    'Should include applied_from at the exact boundary'
);

-- Should include applied_to at the exact boundary
select is(
    pg_temp.found_changes('{"applied_to": "2024-01-03 10:00:00+00"}'),
    array['03', '02', '01'],
    'Should include applied_to at the exact boundary'
);

-- Should keep failed changes when applied_successfully is false
select is(
    pg_temp.found_changes('{"applied_successfully": false}'),
    :'allChangesDesc'::text[],
    'Should keep failed changes when applied_successfully is false'
);

-- Should not execute SQL embedded in the search text
select is(
    pg_temp.found_changes(jsonb_build_object('ts_query_web', v)),
    '{}'::text[],
    format('Should not execute SQL embedded in the search text: %s', v)
)
from unnest(array[
    $$x'); drop table change; --$$,
    $$'x'))) as lexeme union all select 'x'::tsquery, (1/0)::text::tsquery --$$
]) as v;

-- Should require every search term
select is(
    pg_temp.found_changes('{"ts_query_web": "team1 repo1"}'),
    array['02'],
    'Should require every search term'
);

-- Should return a page of results along with the total count of matches
select is(
    pg_temp.found_changes('{"limit": 1, "offset": 1, "organization": "org2"}'),
    array['10'],
    'Should return a page of results'
);
select is(
    (
        select total_count
        from search_changes('{"limit": 1, "offset": 1, "organization": "org2"}')
    ),
    3::bigint,
    'Should return the total count of matches along with a page of results'
);

-- Should return an empty list and a zero count when nothing matches
select results_eq(
    $$
        select changes::text, total_count
        from search_changes('{"organization": "unknown"}')
    $$,
    $$ values ('[]', 0::bigint) $$,
    'Should return an empty list and a zero count when nothing matches'
);

-- Should return no changes for search text with special characters
select is(
    pg_temp.found_changes(jsonb_build_object('ts_query_web', v)),
    '{}'::text[],
    format('Should return no changes for search text with special characters: %s', v)
)
from unnest(array[
    $$'$$,
    '\',
    'cncf.io:443',
    'foo.com/(x)',
    'foo.com/a!b',
    $$foo.com/a'b$$,
    'foo.com:8080',
    'ghcr.io/cncf/app:v1.2',
    'http://foo.com:8080/x'
]) as v;

-- Should return the JSON of a change applied by a merged pull request
select is(
    (
        select (changes->0)::jsonb
        from search_changes('{"limit": 1, "sort_direction": "asc"}')
    ),
    format(
        '{
            "change_id": "%s",
            "applied_at": 1704103200.123456,
            "kind": "repository-added",
            "reconciliation": {
                "reconciliation_id": "%s",
                "completed_at": 1704276000.654321,
                "organization": "org1",
                "pr_created_by": "alice",
                "pr_merged_at": 1704099600.111111,
                "pr_merged_by": "bob",
                "pr_number": 101
            },
            "service": "github",

            "extra": {"repo": {"name": "repo1"}}
        }',
        :'change01ID',
        :'mergedReconciliationID'
    )::jsonb,
    'Should return the JSON of a change applied by a merged pull request'
);

-- Should return the JSON of a change applied by a scheduled reconciliation
select is(
    (
        select (changes->0)::jsonb
        from search_changes('{"kind": ["team-removed"], "service": ["github"]}')
    ),
    format(
        '{
            "change_id": "%s",
            "applied_at": 1704535200.000001,
            "kind": "team-removed",
            "reconciliation": {
                "reconciliation_id": "%s",
                "completed_at": 1704708000.5,
                "organization": "org1"
            },
            "service": "github"
        }',
        :'change06ID',
        :'scheduledReconciliationID'
    )::jsonb,
    'Should return the JSON of a change applied by a scheduled reconciliation'
);

-- Should return the JSON of a change that failed to apply
select is(
    (
        select (changes->0)::jsonb
        from search_changes('{"pr_number": [102], "service": ["github"]}')
    ),
    format(
        '{
            "change_id": "%s",
            "applied_at": 1704362400,
            "kind": "repository-team-added",
            "reconciliation": {
                "reconciliation_id": "%s",
                "completed_at": 1704448800,
                "error": "github: something went wrong",
                "organization": "org1",
                "pr_created_by": "carol",
                "pr_merged_at": 1704362400,
                "pr_merged_by": "dave",
                "pr_number": 102
            },
            "service": "github",

            "error": "team not found",
            "extra": {"repo_name": "repo2", "role": "read", "team_name": "team2"}
        }',
        :'change04ID',
        :'failedReconciliationID'
    )::jsonb,
    'Should return the JSON of a change that failed to apply'
);

-- Should sort by date in ascending order
select is(
    pg_temp.found_changes('{"sort_by": "date", "sort_direction": "asc"}'),
    :'allChangesAsc'::text[],
    'Should sort by date in ascending order'
);

-- Should sort by date in descending order
select is(
    pg_temp.found_changes('{"sort_by": "date", "sort_direction": "desc"}'),
    :'allChangesDesc'::text[],
    'Should sort by date in descending order'
);

-- Should sort by date in descending order by default
select is(
    pg_temp.found_changes('{}'),
    :'allChangesDesc'::text[],
    'Should sort by date in descending order by default'
);

-- ============================================================================
-- CLEANUP
-- ============================================================================

select * from finish();
rollback;
