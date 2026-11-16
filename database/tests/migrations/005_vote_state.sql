-- Tests applying and reverting the vote state migration on existing votes.

-- ============================================================================
-- SETUP
-- ============================================================================

begin;
select plan(15);

-- ============================================================================
-- VARIABLES
-- ============================================================================

\set closedDuplicateVoteID1 '00000000-0000-0000-0000-000000000001'
\set closedDuplicateVoteID2 '00000000-0000-0000-0000-000000000002'
\set closedVoteID '00000000-0000-0000-0000-000000000003'
\set jsonNullVoteID '00000000-0000-0000-0000-000000000004'
\set openVoteID '00000000-0000-0000-0000-000000000005'
\set sqlNullVoteID '00000000-0000-0000-0000-000000000006'

-- Statements of the migration, read from the repository root
\set down `sed '1,/^---- create above/d' database/migrations/schema/005_vote_state.sql`
\set up `sed '/^---- create above/,$d' database/migrations/schema/005_vote_state.sql`

-- ============================================================================
-- SEED DATA
-- ============================================================================

-- Return to the schema the migration is applied on
:down

-- First closed vote in an issue that also has an open vote
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
    :'closedDuplicateVoteID1',
    '{}',
    true,
    'user',
    '2024-01-01T00:00:00Z',
    1,
    false,
    1,
    1,
    'org/repo',
    1,

    '2024-01-01T00:00:00Z',
    '{"passed": true}'
);

-- Second closed vote in an issue that also has an open vote
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
    :'closedDuplicateVoteID2',
    '{}',
    true,
    'user',
    '2024-02-01T00:00:00Z',
    1,
    false,
    1,
    1,
    'org/repo',
    2,

    '2024-02-01T00:00:00Z',
    '{"passed": false}'
);

-- Closed vote with results
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
    '2024-03-01T00:00:00Z',
    1,
    true,
    2,
    2,
    'org/repo',
    3,

    '2024-03-01T00:00:00Z',
    '{"passed": true}'
);

-- Closed vote with results stored as JSON null
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
    :'jsonNullVoteID',
    '{}',
    true,
    'user',
    '2024-04-01T00:00:00Z',
    1,
    false,
    3,
    3,
    'org/repo',
    4,

    '2024-04-01T00:00:00Z',
    'null'
);

-- Open vote in the issue with closed votes
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
    '2024-05-01T00:00:00Z',
    1,
    false,
    1,
    1,
    'org/repo',
    5
);

-- Closed vote with results stored as SQL null
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

    closed_at
) values (
    :'sqlNullVoteID',
    '{}',
    true,
    'user',
    '2024-06-01T00:00:00Z',
    1,
    false,
    4,
    4,
    'org/repo',
    6,

    '2024-06-01T00:00:00Z'
);

-- ============================================================================
-- TESTS
-- ============================================================================

-- Apply the migration and check the migrated votes; the apply then revert lifecycle
-- order is intentional and scenarios are alphabetized within each phase

-- Should apply the migration on existing votes
select lives_ok(:'up', 'Should apply the migration on existing votes');

-- Should convert JSON null results to SQL null
select is(
    (
        select results
        from vote
        where vote_id = :'jsonNullVoteID'
    ),
    null::jsonb,
    'Should convert JSON null results to SQL null'
);

-- Should create the open issue index
select has_index(
    'public',
    'vote',
    'vote_open_issue_idx',
    'Should create the open issue index'
);

-- Should keep all the existing votes
select is(
    (
        select count(*)
        from vote
    ),
    6::bigint,
    'Should keep all the existing votes'
);

-- Should keep closed votes results
select is(
    (
        select results
        from vote
        where vote_id = :'closedVoteID'
    ),
    '{"passed": true}'::jsonb,
    'Should keep closed votes results'
);

-- Should keep SQL null results
select is(
    (
        select results
        from vote
        where vote_id = :'sqlNullVoteID'
    ),
    null::jsonb,
    'Should keep SQL null results'
);

-- Should keep the open vote open
select is(
    (
        select closed
        from vote
        where vote_id = :'openVoteID'
    ),
    false,
    'Should keep the open vote open'
);

-- Should leave closed votes as complete so that they are never reprocessed
select is(
    (
        select count(*)
        from vote
        where closed and close_pending
    ),
    0::bigint,
    'Should leave closed votes as complete so that they are never reprocessed'
);

-- Should leave existing votes not cancelled
select is(
    (
        select count(*)
        from vote
        where cancelled_at is not null
        or cancelled_by is not null
    ),
    0::bigint,
    'Should leave existing votes not cancelled'
);

-- Should leave existing votes without a close head commit
select is(
    (
        select count(*)
        from vote
        where close_head_sha is not null
    ),
    0::bigint,
    'Should leave existing votes without a close head commit'
);

-- Should leave no retries scheduled for existing votes
select is(
    (
        select count(*)
        from vote
        where close_attempts <> 0
        or close_next_attempt_at is not null
    ),
    0::bigint,
    'Should leave no retries scheduled for existing votes'
);

-- Revert the migration and check the reverted schema

-- Should revert the migration
select lives_ok(:'down', 'Should revert the migration');

-- Should keep all the existing votes after reverting
select is(
    (
        select count(*)
        from vote
    ),
    6::bigint,
    'Should keep all the existing votes after reverting'
);

-- Should remove the columns added by the migration
select is(
    (
        select count(*)
        from information_schema.columns
        where table_schema = 'public'
        and table_name = 'vote'
        and column_name in (
            'cancelled_at',
            'cancelled_by',
            'close_announcement_posted_at',
            'close_attempts',
            'close_check_run_created_at',
            'close_comment_posted_at',
            'close_head_sha',
            'close_labels_updated_at',
            'close_next_attempt_at',
            'close_pending'
        )
    ),
    0::bigint,
    'Should remove the columns added by the migration'
);

-- Should remove the open issue index
select hasnt_index(
    'public',
    'vote',
    'vote_open_issue_idx',
    'Should remove the open issue index'
);

-- ============================================================================
-- CLEANUP
-- ============================================================================

select * from finish();
rollback;
