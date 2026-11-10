-- Tests the database schema.

-- ============================================================================
-- SETUP
-- ============================================================================

begin;
select plan(19);

-- ============================================================================
-- VARIABLES
-- ============================================================================

\set closedVoteID '00000000-0000-0000-0000-000000000001'
\set openVoteID '00000000-0000-0000-0000-000000000002'

-- ============================================================================
-- SEED DATA
-- ============================================================================

-- Closed vote in the issue shared with the open vote
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
    vote_comment_id
) values (
    :'closedVoteID',
    '{}',
    true,
    'user',
    current_timestamp,
    1,
    false,
    1,
    1,
    'org/repo',
    1
);

-- Open vote in the issue shared with the closed vote
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
    current_timestamp,
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

-- Test: extensions
select has_extension('pgcrypto');

-- Test: functions
select has_function('public', 'string_to_interval', array['text']);

-- Test: tables
select tables_are(
    'public',
    array[
        'version_functions',
        'version_schema',
        'vote'
    ]
);

-- Test: vote check constraints
select results_eq(
    $$
        select
            conname::text collate "default",
            pg_get_constraintdef(oid) collate "default"
        from pg_constraint
        where conrelid = 'public.vote'::regclass
        and contype = 'c'
        order by conname collate "C"
    $$,
    $$
        values
            ('vote_cancelled_by_chk', 'CHECK (((cancelled_at IS NULL) = (cancelled_by IS NULL)))'),
            ('vote_cancelled_closed_chk', 'CHECK (((cancelled_at IS NULL) OR closed))'),
            ('vote_close_attempts_chk', 'CHECK ((close_attempts >= 0))'),
            ('vote_close_pending_chk', 'CHECK (((NOT close_pending) OR closed))')
    $$,
    'Vote should have the expected check constraints'
);

-- Test: vote columns
select columns_are(
    'public',
    'vote',
    array[
        'vote_id',
        'cfg',
        'close_attempts',
        'close_pending',
        'closed',
        'created_at',
        'created_by',
        'ends_at',
        'installation_id',
        'is_pull_request',
        'issue_id',
        'issue_number',
        'repository_full_name',
        'vote_comment_id',

        'cancelled_at',
        'cancelled_by',
        'checked_at',
        'close_announcement_posted_at',
        'close_check_run_created_at',
        'close_comment_posted_at',
        'close_head_sha',
        'close_labels_updated_at',
        'close_next_attempt_at',
        'closed_at',
        'issue_title',
        'organization',
        'results'
    ]
);

-- Test: vote columns definition
select results_eq(
    $$
        select
            column_name::text collate "default",
            data_type::text collate "default",
            is_nullable::text collate "default",
            column_default::text collate "default"
        from information_schema.columns
        where table_schema = 'public'
        and table_name = 'vote'
        order by column_name collate "C"
    $$,
    $$
        values
            ('cancelled_at', 'timestamp with time zone', 'YES', null),
            ('cancelled_by', 'text', 'YES', null),
            ('cfg', 'jsonb', 'NO', null),
            ('checked_at', 'timestamp with time zone', 'YES', null),
            ('close_announcement_posted_at', 'timestamp with time zone', 'YES', null),
            ('close_attempts', 'integer', 'NO', '0'),
            ('close_check_run_created_at', 'timestamp with time zone', 'YES', null),
            ('close_comment_posted_at', 'timestamp with time zone', 'YES', null),
            ('close_head_sha', 'text', 'YES', null),
            ('close_labels_updated_at', 'timestamp with time zone', 'YES', null),
            ('close_next_attempt_at', 'timestamp with time zone', 'YES', null),
            ('close_pending', 'boolean', 'NO', 'false'),
            ('closed', 'boolean', 'NO', 'false'),
            ('closed_at', 'timestamp with time zone', 'YES', null),
            ('created_at', 'timestamp with time zone', 'NO', 'CURRENT_TIMESTAMP'),
            ('created_by', 'text', 'NO', null),
            ('ends_at', 'timestamp with time zone', 'NO', null),
            ('installation_id', 'bigint', 'NO', null),
            ('is_pull_request', 'boolean', 'NO', null),
            ('issue_id', 'bigint', 'NO', null),
            ('issue_number', 'bigint', 'NO', null),
            ('issue_title', 'text', 'YES', null),
            ('organization', 'text', 'YES', null),
            ('repository_full_name', 'text', 'NO', null),
            ('results', 'jsonb', 'YES', null),
            ('vote_comment_id', 'bigint', 'NO', null),
            ('vote_id', 'uuid', 'NO', 'gen_random_uuid()')
    $$,
    'Vote columns should have the expected types, nullability and defaults'
);

-- Test: vote indexes
select indexes_are('public', 'vote', array['vote_open_issue_idx', 'vote_pkey']);

-- Test: vote open issue index definition
select is(
    pg_get_indexdef('public.vote_open_issue_idx'::regclass),
    'CREATE UNIQUE INDEX vote_open_issue_idx ON public.vote USING btree '
    '(repository_full_name, issue_number) WHERE (closed = false)',
    'Vote open issue index should be unique, partial and on the expected columns'
);

-- Test: vote primary key
select col_is_pk('public', 'vote', 'vote_id', 'Vote id should be the primary key');

-- Should allow a cancelled closed vote
select lives_ok(
    $$
        insert into vote (
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

            cancelled_at,
            cancelled_by
        ) values (
            '{}',
            true,
            'user',
            current_timestamp,
            1,
            false,
            2,
            2,
            'org/repo',
            6,

            current_timestamp,
            'user'
        )
    $$,
    'Should allow a cancelled closed vote'
);

-- Should allow an open vote in the same issue number of another repository
select lives_ok(
    $$
        insert into vote (
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
            '{}',
            'user',
            current_timestamp,
            1,
            false,
            1,
            1,
            'org/other-repo',
            4
        )
    $$,
    'Should allow an open vote in the same issue number of another repository'
);

-- Should allow another closed vote in an issue with an open vote
select lives_ok(
    $$
        insert into vote (
            cfg,
            closed,
            created_by,
            ends_at,
            installation_id,
            is_pull_request,
            issue_id,
            issue_number,
            repository_full_name,
            vote_comment_id
        ) values (
            '{}',
            true,
            'user',
            current_timestamp,
            1,
            false,
            1,
            1,
            'org/repo',
            3
        )
    $$,
    'Should allow another closed vote in an issue with an open vote'
);

-- Should allow close pending on a closed vote
select lives_ok(
    format(
        $$
            update vote
            set close_pending = true
            where vote_id = %L
        $$,
        :'closedVoteID'
    ),
    'Should allow close pending on a closed vote'
);

-- Should reject a cancellation time without the user who cancelled the vote
select throws_ok(
    $$
        insert into vote (
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

            cancelled_at
        ) values (
            '{}',
            true,
            'user',
            current_timestamp,
            1,
            false,
            2,
            2,
            'org/repo',
            7,

            current_timestamp
        )
    $$,
    '23514',
    'new row for relation "vote" violates check constraint "vote_cancelled_by_chk"',
    'Should reject a cancellation time without the user who cancelled the vote'
);

-- Should reject a cancelled open vote
select throws_ok(
    $$
        insert into vote (
            cfg,
            created_by,
            ends_at,
            installation_id,
            is_pull_request,
            issue_id,
            issue_number,
            repository_full_name,
            vote_comment_id,

            cancelled_at,
            cancelled_by
        ) values (
            '{}',
            'user',
            current_timestamp,
            1,
            false,
            3,
            3,
            'org/repo',
            8,

            current_timestamp,
            'user'
        )
    $$,
    '23514',
    'new row for relation "vote" violates check constraint "vote_cancelled_closed_chk"',
    'Should reject a cancelled open vote'
);

-- Should reject a negative number of close attempts
select throws_ok(
    format(
        $$
            update vote
            set close_attempts = -1
            where vote_id = %L
        $$,
        :'openVoteID'
    ),
    '23514',
    'new row for relation "vote" violates check constraint "vote_close_attempts_chk"',
    'Should reject a negative number of close attempts'
);

-- Should reject a second open vote in an issue
select throws_ok(
    $$
        insert into vote (
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
            '{}',
            'user',
            current_timestamp,
            1,
            false,
            1,
            1,
            'org/repo',
            5
        )
    $$,
    '23505',
    'duplicate key value violates unique constraint "vote_open_issue_idx"',
    'Should reject a second open vote in an issue'
);

-- Should reject a user who cancelled the vote without a cancellation time
select throws_ok(
    $$
        insert into vote (
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

            cancelled_by
        ) values (
            '{}',
            true,
            'user',
            current_timestamp,
            1,
            false,
            2,
            2,
            'org/repo',
            9,

            'user'
        )
    $$,
    '23514',
    'new row for relation "vote" violates check constraint "vote_cancelled_by_chk"',
    'Should reject a user who cancelled the vote without a cancellation time'
);

-- Should reject close pending on an open vote
select throws_ok(
    format(
        $$
            update vote
            set close_pending = true
            where vote_id = %L
        $$,
        :'openVoteID'
    ),
    '23514',
    'new row for relation "vote" violates check constraint "vote_close_pending_chk"',
    'Should reject close pending on an open vote'
);

-- ============================================================================
-- CLEANUP
-- ============================================================================

select * from finish();
rollback;
