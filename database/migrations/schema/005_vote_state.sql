-- Tracks closing and cancellation side effects and allows only one open vote per issue
-- or pull request.

-- A vote without results is stored as SQL null, never as JSON null
update vote
set results = null
where results = 'null'::jsonb;

-- close_pending is set when a vote is closed and cleared once all its side effects are
-- done, so its default marks every vote closed before this migration as complete. Being
-- opt-in also covers the votes closed by a previous version while it is being replaced.
-- close_head_sha is the head of the pull request when the vote was closed, which is the
-- commit that gets the closing check run. It is null in votes on issues, as well as when
-- the head could not be read when closing the vote. cancelled_at and cancelled_by are
-- set when a vote is cancelled, which also closes it and leaves its side effects
-- pending: the vote is deleted once its cancellation has been published, or once
-- publishing it runs out of attempts and it is abandoned.
alter table vote
    add column close_attempts integer not null default 0,
    add column close_pending boolean not null default false,

    add column cancelled_at timestamptz,
    add column cancelled_by text,
    add column close_announcement_posted_at timestamptz,
    add column close_check_run_created_at timestamptz,
    add column close_comment_posted_at timestamptz,
    add column close_head_sha text,
    add column close_labels_updated_at timestamptz,
    add column close_next_attempt_at timestamptz,

    add constraint vote_cancelled_by_chk check (
        (cancelled_at is null) = (cancelled_by is null)
    ),
    add constraint vote_cancelled_closed_chk check (cancelled_at is null or closed),
    add constraint vote_close_attempts_chk check (close_attempts >= 0),
    add constraint vote_close_pending_chk check (not close_pending or closed);

-- If this statement fails with a unique violation, some issue or pull request has more
-- than one open vote, and the whole migration is rolled back. Duplicates are not
-- resolved automatically as that needs a decision about which vote to keep. They can be
-- listed with the query below. To resolve them, delete all but one of the rows of each
-- issue or pull request, and the vote comments of the deleted rows, then migrate again.
--
--   select repository_full_name, issue_number, array_agg(vote_id order by created_at)
--   from vote
--   where closed = false
--   group by repository_full_name, issue_number
--   having count(*) > 1;
create unique index vote_open_issue_idx on vote (repository_full_name, issue_number)
where closed = false;

---- create above / drop below ----

drop index vote_open_issue_idx;

-- The previous schema has no cancelled votes, as they were deleted when cancelled. The
-- ones kept here until their cancellation is published or abandoned would otherwise be
-- left as closed votes without results once these columns are dropped.
delete from vote
where cancelled_at is not null;

alter table vote
    drop constraint vote_cancelled_by_chk,
    drop constraint vote_cancelled_closed_chk,
    drop constraint vote_close_attempts_chk,
    drop constraint vote_close_pending_chk,

    drop column cancelled_at,
    drop column cancelled_by,
    drop column close_announcement_posted_at,
    drop column close_attempts,
    drop column close_check_run_created_at,
    drop column close_comment_posted_at,
    drop column close_head_sha,
    drop column close_labels_updated_at,
    drop column close_next_attempt_at,
    drop column close_pending;
