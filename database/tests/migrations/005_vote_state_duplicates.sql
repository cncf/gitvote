-- Tests applying the vote state migration when an issue has more than one open vote.

-- ============================================================================
-- SETUP
-- ============================================================================

begin;
select plan(1);

-- ============================================================================
-- VARIABLES
-- ============================================================================

\set firstOpenVoteID '00000000-0000-0000-0000-000000000001'
\set secondOpenVoteID '00000000-0000-0000-0000-000000000002'

-- Statements of the migration, read from the repository root
\set down `sed '1,/^---- create above/d' database/migrations/schema/005_vote_state.sql`
\set up `sed '/^---- create above/,$d' database/migrations/schema/005_vote_state.sql`

-- ============================================================================
-- SEED DATA
-- ============================================================================

-- Return to the schema the migration is applied on
:down

-- First open vote in the same issue
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
    :'firstOpenVoteID',
    '{}',
    'user',
    '2024-01-01T00:00:00Z',
    1,
    false,
    1,
    1,
    'org/repo',
    1
);

-- Second open vote in the same issue
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
    :'secondOpenVoteID',
    '{}',
    'user',
    '2024-01-01T00:00:00Z',
    1,
    false,
    1,
    1,
    'org/repo',
    2
);

-- ============================================================================
-- TESTS
-- ============================================================================

-- Should fail when an issue has more than one open vote
select throws_ok(
    :'up',
    '23505',
    'could not create unique index "vote_open_issue_idx"',
    'Should fail when an issue has more than one open vote'
);

-- ============================================================================
-- CLEANUP
-- ============================================================================

select * from finish();
rollback;
