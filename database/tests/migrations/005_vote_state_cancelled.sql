-- Tests reverting the vote state migration when some votes are cancelled.

-- ============================================================================
-- SETUP
-- ============================================================================

begin;
select plan(2);

-- ============================================================================
-- VARIABLES
-- ============================================================================

\set cancelledVoteID '00000000-0000-0000-0000-000000000001'
\set closedVoteID '00000000-0000-0000-0000-000000000002'
\set openVoteID '00000000-0000-0000-0000-000000000003'
\set pendingVoteID '00000000-0000-0000-0000-000000000004'

-- Statements of the migration, read from the repository root
\set down `sed '1,/^---- create above/d' database/migrations/schema/005_vote_state.sql`

-- ============================================================================
-- SEED DATA
-- ============================================================================

-- Cancelled vote whose cancellation is not published yet
insert into vote (
    vote_id,
    cfg,
    close_pending,
    closed,
    created_by,
    ends_at,
    installation_id,
    is_pull_request,
    issue_id,
    issue_number,
    repository_full_name,
    vote_comment_id,

    cancelled_at,
    cancelled_by,
    closed_at
) values (
    :'cancelledVoteID',
    '{}',
    true,
    true,
    'user',
    '2024-01-01T00:00:00Z',
    1,
    true,
    1,
    1,
    'org/repo',
    1,

    '2024-01-01T00:00:00Z',
    'user',
    '2024-01-01T00:00:00Z'
);

-- Closed vote with results in the same pull request
insert into vote (
    vote_id,
    cfg,
    closed,
    created_by,
    ends_at,
    installation_id,
    is_pull_request,
    issue_id,
    issue_number,
    repository_full_name,
    vote_comment_id,

    closed_at,
    results
) values (
    :'closedVoteID',
    '{}',
    true,
    'user',
    '2024-02-01T00:00:00Z',
    1,
    true,
    1,
    1,
    'org/repo',
    2,

    '2024-02-01T00:00:00Z',
    '{"passed": true}'
);

-- Open vote in the same pull request
insert into vote (
    vote_id,
    cfg,
    created_by,
    ends_at,
    installation_id,
    is_pull_request,
    issue_id,
    issue_number,
    repository_full_name,
    vote_comment_id
) values (
    :'openVoteID',
    '{}',
    'user',
    '2024-03-01T00:00:00Z',
    1,
    true,
    1,
    1,
    'org/repo',
    3
);

-- Closed vote without results whose side effects are pending in the same pull request
insert into vote (
    vote_id,
    cfg,
    close_pending,
    closed,
    created_by,
    ends_at,
    installation_id,
    is_pull_request,
    issue_id,
    issue_number,
    repository_full_name,
    vote_comment_id,

    closed_at
) values (
    :'pendingVoteID',
    '{}',
    true,
    true,
    'user',
    '2024-02-15T00:00:00Z',
    1,
    true,
    1,
    1,
    'org/repo',
    4,

    '2024-02-15T00:00:00Z'
);

-- ============================================================================
-- TESTS
-- ============================================================================

-- Should revert the migration with cancelled votes
select lives_ok(:'down', 'Should revert the migration with cancelled votes');

-- Should delete only the cancelled votes
select results_eq(
    $$
        select vote_id
        from vote
        order by vote_id
    $$,
    format(
        $$values (%L::uuid), (%L::uuid), (%L::uuid)$$,
        :'closedVoteID',
        :'openVoteID',
        :'pendingVoteID'
    ),
    'Should delete only the cancelled votes'
);

-- ============================================================================
-- CLEANUP
-- ============================================================================

select * from finish();
rollback;
