//! This module defines an abstraction layer over the database.

use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use deadpool_postgres::{GenericClient, Pool};
#[cfg(test)]
use mockall::automock;
use thiserror::Error;
use time::OffsetDateTime;
use tokio_postgres::types::Json;
use tracing::warn;
use uuid::Uuid;

use crate::{
    cfg_repo::CfgProfile,
    cmd::{CheckVoteInput, CreateVoteInput},
    results::{Vote, VoteResults},
};

#[cfg(test)]
mod tests;

/// How long a claimed vote is reserved for the worker that claimed it, in seconds. If
/// the worker dies the vote is picked up again after this time, and no other worker can
/// claim it until then. It must be longer than a closing attempt takes: the processor
/// stops attempts that take longer than its attempt timeout, which a compile time
/// assertion keeps well below this duration.
pub(crate) const CLOSE_CLAIM_DURATION_SECS: i64 = 1800;

/// Number of failed attempts after which the closing side effects of a closed vote, or
/// the publication of a cancellation, are no longer retried. With the delays below the
/// last attempt happens after about 20 hours. Closing a vote restarts its attempts, so
/// its side effects get them all. Finished votes still open have no limit, as only
/// closing them publishes their results: they are retried at the maximum delay.
pub(crate) const CLOSE_MAX_ATTEMPTS: i32 = 12;

/// Delay before retrying a failed attempt to close a vote, doubled on each attempt.
const CLOSE_RETRY_BASE_DELAY_SECS: f64 = 60.0;

/// Maximum delay before retrying a failed attempt to close a vote.
const CLOSE_RETRY_MAX_DELAY_SECS: f64 = 21600.0;

/// Highest number of times the base delay is doubled. The maximum delay is reached well
/// before it, and capping it keeps the power from overflowing as attempts grow.
const CLOSE_RETRY_MAX_EXPONENT: i32 = 30;

/// Trait that defines some operations a DB implementation must support.
#[async_trait]
#[allow(clippy::ref_option_ref)]
#[cfg_attr(test, automock)]
pub(crate) trait DB {
    /// Cancels the open vote (if any) in the issue/pr provided, returning it. The vote
    /// is closed and marked as cancelled right away, with the publication of the
    /// cancellation pending and claimed by the caller, so that the votes closer only
    /// takes over if the caller fails to publish it. Once published, the vote is
    /// deleted (see [`DB::complete_vote_cancellation`]), as well as when its publication
    /// has no attempts left (see [`DB::record_vote_close_failure`]).
    async fn cancel_vote(
        &self,
        repository_full_name: &str,
        issue_number: i64,
        cancelled_by: &str,
    ) -> Result<Option<Vote>>;

    /// Claims the vote that has been waiting the longest for closing work: a finished
    /// vote to close, a closed vote with side effects pending or a cancelled vote whose
    /// cancellation has not been published yet. Votes waiting for a retry are not
    /// selected, nor closed votes without attempts left. Finished votes still open are
    /// selected however many attempts failed. A claim lasts until the outcome is
    /// recorded, or expires if that never happens.
    async fn claim_vote_to_close(&self) -> Result<Option<Vote>>;

    /// Closes the vote provided storing its results and the head commit of its pull
    /// request (if known), leaving its closing side effects pending. Its failed attempts
    /// are restarted, so the side effects get all of them, and its claim is kept. It
    /// returns false if the vote is not open anymore.
    async fn close_vote(
        &self,
        vote_id: Uuid,
        results: Option<&VoteResults>,
        head_sha: Option<&str>,
    ) -> Result<bool>;

    /// Deletes the cancelled vote provided once its cancellation has been published, as
    /// nothing else needs it. Votes not cancelled are never deleted.
    async fn complete_vote_cancellation(&self, vote_id: Uuid) -> Result<()>;

    /// Marks the closing side effects of the vote provided as complete.
    async fn complete_vote_close(&self, vote_id: Uuid) -> Result<()>;

    /// Gets open vote (if available) in the issue/pr provided.
    async fn get_open_vote(&self, repository_full_name: &str, issue_number: i64) -> Result<Option<Vote>>;

    /// Gets open votes that have close on passing enabled.
    async fn get_open_votes_with_close_on_passing(&self) -> Result<Vec<Vote>>;

    /// Gets pending status checks.
    async fn get_pending_status_checks(&self) -> Result<Vec<CheckVoteInput>>;

    /// Gets vote by id. Cancelled votes are not returned, even while their cancellation
    /// is pending publication.
    async fn get_vote(&self, vote_id: Uuid) -> Result<Option<Vote>>;

    /// Checks if the issue/pr provided has a vote that owns its check. A cancelled vote
    /// only counts until the check run step of its cancellation is done, as nothing
    /// publishes the check of the pull request heads pushed after that. It stops
    /// counting as well when its cancellation is abandoned, as the vote is deleted then.
    async fn has_vote(&self, repository_full_name: &str, issue_number: i64) -> Result<bool>;

    /// Checks if the issue/pr provided already has a vote open.
    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool>;

    /// Checks if a vote referencing the vote comment provided exists.
    async fn has_vote_with_comment(&self, vote_comment_id: i64) -> Result<bool>;

    /// Checks if the check run or labels step provided of the vote still has to publish
    /// its state, according to the vote stored rather than to the one the caller read.
    /// It does not once it has been recorded as done, or if the vote does not exist
    /// anymore, as well as if a newer vote exists in the same issue/pr, either open,
    /// closed or cancelled with its cancellation pending publication. Newer votes
    /// already deleted are not seen, but storing a vote marks these steps of the older
    /// votes as done (see [`DB::store_vote`]). Other steps are never superseded, so
    /// checking them returns an error.
    async fn is_close_state_step_pending(&self, vote_id: Uuid, step: VoteCloseStep) -> Result<bool>;

    /// Lists all votes stored for the provided repository. Cancelled votes are not
    /// listed, even while their cancellation is pending publication.
    async fn list_votes(&self, repository_full_name: &str) -> Result<Vec<Vote>>;

    /// Records a failed attempt to close the vote provided, or to publish its
    /// cancellation, scheduling the next one with an exponential backoff. Votes still
    /// open are always rescheduled. When a cancelled vote runs out of attempts, its
    /// cancellation is abandoned and the vote is deleted, leaving the steps still
    /// missing unpublished, so the issue/pr behaves as if it had no vote. The failure is
    /// only recorded while the caller still holds the claim it got the vote with, which
    /// is identified by the time it ends at: a claim expired and claimed again, or taken
    /// over by the cancellation of the vote, belongs to another worker, whose attempts
    /// must not be rescheduled.
    async fn record_vote_close_failure(
        &self,
        vote_id: Uuid,
        claimed_until: OffsetDateTime,
    ) -> Result<VoteCloseFailureOutcome>;

    /// Records that the closing or cancellation side effect provided is done, so it is
    /// not repeated. A step recorded already keeps the time it was first recorded at.
    async fn record_vote_close_step(&self, vote_id: Uuid, step: VoteCloseStep) -> Result<()>;

    /// Stores the vote provided in the database. If the issue/pr has a vote open
    /// already the error returned is a [`VoteAlreadyOpenError`]. The new vote
    /// supersedes the state of the older votes in the issue/pr, so their check run and
    /// labels steps still pending are marked as done without publishing anything.
    async fn store_vote(
        &self,
        vote_comment_id: i64,
        input: &CreateVoteInput,
        cfg: &CfgProfile,
    ) -> Result<Uuid>;

    /// Finishes the vote provided now, unless it has finished already: reactions are
    /// only counted until the end of the vote, so that end must not move forward.
    async fn update_vote_ends_at(&self, vote_id: Uuid) -> Result<()>;

    /// Updates vote's last check ts.
    async fn update_vote_last_check(&self, vote_id: Uuid) -> Result<()>;
}

/// Type alias to represent a DB trait object.
pub(crate) type DynDB = Arc<dyn DB + Send + Sync>;

/// Error returned when storing a vote in an issue/pr that has a vote open already.
#[derive(Debug, Error)]
#[error("the issue or pull request has a vote open already")]
pub(crate) struct VoteAlreadyOpenError;

/// Outcome of recording a failed attempt to close a vote or to publish its cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoteCloseFailureOutcome {
    /// The failure was the last attempt to publish the cancellation of the vote, so the
    /// cancellation was abandoned and the vote deleted.
    CancellationAbandoned,
    /// Nothing was recorded, as the caller does not hold the claim of the vote anymore
    /// or the vote does not exist.
    ClaimLost,
    /// The failure was recorded, and there are no attempts left for the closing side
    /// effects of the vote, which was closed already.
    NoAttemptsLeft,
    /// The failure was recorded, and the next attempt has been scheduled.
    RetryScheduled,
}

/// Side effects of closing or cancelling a vote that are tracked individually. A step is
/// recorded once it needs no more work, even if nothing was published: GitHub may have
/// rejected the announcement, and the check run and labels are skipped when a newer vote
/// exists or has existed (storing a vote marks them as done in the older votes).
/// Cancellations only use the comment, check run and labels steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoteCloseStep {
    /// Results announcement posted to discussions.
    Announcement,
    /// Closing check run created.
    CheckRun,
    /// Results comment posted.
    Comment,
    /// Closing labels updated.
    Labels,
}

/// DB implementation backed by `PostgreSQL`.
pub(crate) struct PgDB {
    /// Pool of database connections.
    pool: Pool,
}

impl PgDB {
    /// Creates a new `PgDB` instance.
    pub(crate) fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DB for PgDB {
    /// [`DB::cancel_vote`].
    async fn cancel_vote(
        &self,
        repository_full_name: &str,
        issue_number: i64,
        cancelled_by: &str,
    ) -> Result<Option<Vote>> {
        // Close and mark as cancelled the open vote (if any), claiming the publication
        // of its cancellation in the same statement
        let db = self.pool.get().await?;
        let vote = db
            .query_opt(
                "
                update vote
                set
                    cancelled_at = current_timestamp,
                    cancelled_by = $3::text,
                    close_attempts = 0,
                    close_next_attempt_at = current_timestamp + make_interval(secs => $4::bigint),
                    close_pending = true,
                    closed = true,
                    closed_at = current_timestamp
                where repository_full_name = $1::text
                and issue_number = $2::bigint
                and closed = false
                returning *
                ",
                &[
                    &repository_full_name,
                    &issue_number,
                    &cancelled_by,
                    &CLOSE_CLAIM_DURATION_SECS,
                ],
            )
            .await?
            .map(|row| Vote::try_from(&row))
            .transpose()?;
        Ok(vote)
    }

    /// [`DB::claim_vote_to_close`].
    async fn claim_vote_to_close(&self) -> Result<Option<Vote>> {
        // Claim the next vote with closing work due. Cancelled votes whose cancellation
        // is pending publication are included, as they are closed with close pending
        // set. The attempts limit only applies to closed votes: finished votes still
        // open are retried until they close, as only that publishes their results
        let db = self.pool.get().await?;
        let Some(row) = db
            .query_opt(
                "
                update vote v
                set close_next_attempt_at = current_timestamp + make_interval(secs => $1::bigint)
                where v.vote_id = (
                    select c.vote_id
                    from vote c
                    where (
                        (c.close_pending and c.close_attempts < $2::integer)
                        or (c.closed = false and current_timestamp > c.ends_at)
                    )
                    and (c.close_next_attempt_at is null or c.close_next_attempt_at <= current_timestamp)
                    order by coalesce(c.close_next_attempt_at, c.ends_at)
                    for update skip locked
                    limit 1
                )
                returning *
                ",
                &[&CLOSE_CLAIM_DURATION_SECS, &CLOSE_MAX_ATTEMPTS],
            )
            .await?
        else {
            return Ok(None);
        };

        // The worker never gets a vote that cannot be read, so its failure is recorded
        // here to back off its next claim, which stops once a closed vote has no
        // attempts left. The connection held is reused, as waiting for another one could
        // deadlock a pool of one connection
        match Vote::try_from(&row) {
            Ok(vote) => Ok(Some(vote)),
            Err(err) => {
                // Identify the vote in the error, as the caller cannot tell which it is
                let vote_id: Uuid = row.try_get("vote_id")?;
                let err = err.context(format!("error decoding claimed vote {vote_id}"));

                // Record the failure, keeping the decode error as the one returned if
                // that fails too
                let record_result = async {
                    let claimed_until = row.try_get("close_next_attempt_at")?;
                    record_vote_close_failure_with(&db, vote_id, claimed_until).await
                }
                .await;

                // Leave a trace of the attempts running out, as nothing else reports it
                match record_result {
                    Ok(VoteCloseFailureOutcome::CancellationAbandoned) => {
                        Err(err.context("cancellation abandoned, no attempts left"))
                    }
                    Ok(VoteCloseFailureOutcome::NoAttemptsLeft) => Err(err.context("no attempts left")),
                    Ok(VoteCloseFailureOutcome::ClaimLost | VoteCloseFailureOutcome::RetryScheduled) => {
                        Err(err)
                    }
                    Err(record_err) => {
                        warn!(
                            ?record_err,
                            %vote_id,
                            "error recording failure of undecodable claimed vote"
                        );
                        Err(err)
                    }
                }
            }
        }
    }

    /// [`DB::close_vote`].
    async fn close_vote(
        &self,
        vote_id: Uuid,
        results: Option<&VoteResults>,
        head_sha: Option<&str>,
    ) -> Result<bool> {
        // Close the vote only while it is still open, restarting its attempts
        let db = self.pool.get().await?;
        let closed = db
            .execute(
                "
                update vote
                set
                    close_attempts = 0,
                    close_head_sha = $1::text,
                    close_pending = true,
                    closed = true,
                    closed_at = current_timestamp,
                    results = $2::jsonb
                where vote_id = $3::uuid
                and closed = false;
                ",
                &[&head_sha, &results.map(Json), &vote_id],
            )
            .await?;
        Ok(closed == 1)
    }

    /// [`DB::complete_vote_cancellation`].
    async fn complete_vote_cancellation(&self, vote_id: Uuid) -> Result<()> {
        // Delete the vote only if it was cancelled
        let db = self.pool.get().await?;
        db.execute(
            "
            delete from vote
            where vote_id = $1::uuid
            and cancelled_at is not null;
            ",
            &[&vote_id],
        )
        .await?;
        Ok(())
    }

    /// [`DB::complete_vote_close`].
    async fn complete_vote_close(&self, vote_id: Uuid) -> Result<()> {
        let db = self.pool.get().await?;
        db.execute(
            "
            update vote
            set
                close_next_attempt_at = null,
                close_pending = false
            where vote_id = $1::uuid;
            ",
            &[&vote_id],
        )
        .await?;
        Ok(())
    }

    /// [`DB::get_open_vote`].
    async fn get_open_vote(&self, repository_full_name: &str, issue_number: i64) -> Result<Option<Vote>> {
        let db = self.pool.get().await?;
        let vote = db
            .query_opt(
                "
                select *
                from vote
                where repository_full_name = $1::text
                and issue_number = $2::bigint
                and closed = false
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?
            .map(|row| Vote::try_from(&row))
            .transpose()?;
        Ok(vote)
    }

    /// [`DB::get_open_votes_with_close_on_passing`].
    async fn get_open_votes_with_close_on_passing(&self) -> Result<Vec<Vote>> {
        // Cancelled votes are closed, so they are never selected
        let db = self.pool.get().await?;
        let votes = db
            .query(
                "
                select v.*
                from vote v
                cross join lateral string_to_interval(v.cfg->>'close_on_passing_min_wait') as min_wait
                where v.closed = false
                and v.cfg->'close_on_passing' = 'true'::jsonb
                and (
                    v.cfg->>'close_on_passing_min_wait' is null
                    or (
                        min_wait > '0'::interval
                        and current_timestamp - v.created_at > min_wait
                    )
                )
                ",
                &[],
            )
            .await?
            .iter()
            .map(Vote::try_from)
            .collect::<Result<_>>()?;
        Ok(votes)
    }

    /// [`DB::get_pending_status_checks`].
    async fn get_pending_status_checks(&self) -> Result<Vec<CheckVoteInput>> {
        // Cancelled votes are closed, so they are never selected
        let db = self.pool.get().await?;
        let inputs = db
            .query(
                "
                select v.issue_number, v.repository_full_name
                from vote v
                cross join lateral string_to_interval(v.cfg->>'periodic_status_check') as check_period
                where v.closed = false
                and check_period >= '1 day'::interval
                and current_timestamp - v.created_at > check_period
                and (v.checked_at is null or current_timestamp - v.checked_at > check_period)
                and v.ends_at > current_timestamp + '1 hour'::interval
                ",
                &[],
            )
            .await?
            .iter()
            .map(|row| CheckVoteInput {
                issue_number: row.get("issue_number"),
                repository_full_name: row.get("repository_full_name"),

                installation_id: None,
            })
            .collect();
        Ok(inputs)
    }

    /// [`DB::get_vote`].
    async fn get_vote(&self, vote_id: Uuid) -> Result<Option<Vote>> {
        // Cancelled votes are left out, as they are only kept until their cancellation
        // has been published
        let db = self.pool.get().await?;
        let vote = db
            .query_opt(
                "
                select *
                from vote
                where vote_id = $1::uuid
                and cancelled_at is null
                ",
                &[&vote_id],
            )
            .await?
            .map(|row| Vote::try_from(&row))
            .transpose()?;
        Ok(vote)
    }

    /// [`DB::has_vote`].
    async fn has_vote(&self, repository_full_name: &str, issue_number: i64) -> Result<bool> {
        // Cancelled votes pending publication count until their check run step is done
        let db = self.pool.get().await?;
        let has_vote = db
            .query_one(
                "
                select exists (
                    select 1
                    from vote
                    where repository_full_name = $1::text
                    and issue_number = $2::bigint
                    and not (
                        cancelled_at is not null
                        and close_check_run_created_at is not null
                    )
                )
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?
            .get(0);
        Ok(has_vote)
    }

    /// [`DB::has_vote_open`].
    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool> {
        // Cancelled votes are closed, so a new vote can be created while the
        // cancellation is pending publication
        let db = self.pool.get().await?;
        let has_vote_open = db
            .query_one(
                "
                select exists (
                    select 1
                    from vote
                    where repository_full_name = $1::text
                    and issue_number = $2::bigint
                    and closed = false
                )
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?
            .get(0);
        Ok(has_vote_open)
    }

    /// [`DB::has_vote_with_comment`].
    async fn has_vote_with_comment(&self, vote_comment_id: i64) -> Result<bool> {
        // Cancelled votes pending publication count, as they were stored and their vote
        // comment must be kept
        let db = self.pool.get().await?;
        let has_vote_with_comment = db
            .query_one(
                "
                select exists (
                    select 1
                    from vote
                    where vote_comment_id = $1::bigint
                )
                ",
                &[&vote_comment_id],
            )
            .await?
            .get(0);
        Ok(has_vote_with_comment)
    }

    /// [`DB::is_close_state_step_pending`].
    async fn is_close_state_step_pending(&self, vote_id: Uuid, step: VoteCloseStep) -> Result<bool> {
        // Pick the query checking the step provided, as only the state steps can be
        // superseded. Cancelled votes pending publication count as newer votes, as their
        // own state supersedes the state of the older votes
        let query = match step {
            VoteCloseStep::Announcement | VoteCloseStep::Comment => {
                bail!("only the check run and labels steps publish the state of a vote")
            }
            VoteCloseStep::CheckRun => {
                "
                select exists (
                    select 1
                    from vote v
                    where v.vote_id = $1::uuid
                    and v.close_check_run_created_at is null
                    and not exists (
                        select 1
                        from vote newer
                        where newer.repository_full_name = v.repository_full_name
                        and newer.issue_number = v.issue_number
                        and newer.created_at > v.created_at
                    )
                )
                "
            }
            VoteCloseStep::Labels => {
                "
                select exists (
                    select 1
                    from vote v
                    where v.vote_id = $1::uuid
                    and v.close_labels_updated_at is null
                    and not exists (
                        select 1
                        from vote newer
                        where newer.repository_full_name = v.repository_full_name
                        and newer.issue_number = v.issue_number
                        and newer.created_at > v.created_at
                    )
                )
                "
            }
        };

        // Check the step against the vote stored
        let db = self.pool.get().await?;
        let is_pending = db.query_one(query, &[&vote_id]).await?.get(0);
        Ok(is_pending)
    }

    /// [`DB::list_votes`].
    async fn list_votes(&self, repository_full_name: &str) -> Result<Vec<Vote>> {
        // Cancelled votes are left out, as they are only kept until their cancellation
        // has been published
        let db = self.pool.get().await?;
        let votes = db
            .query(
                "
                select *
                from vote
                where repository_full_name = $1::text
                and cancelled_at is null
                order by created_at desc
                ",
                &[&repository_full_name],
            )
            .await?
            .iter()
            .map(Vote::try_from)
            .collect::<Result<_>>()?;
        Ok(votes)
    }

    /// [`DB::record_vote_close_failure`].
    async fn record_vote_close_failure(
        &self,
        vote_id: Uuid,
        claimed_until: OffsetDateTime,
    ) -> Result<VoteCloseFailureOutcome> {
        let db = self.pool.get().await?;
        record_vote_close_failure_with(&db, vote_id, claimed_until).await
    }

    /// [`DB::record_vote_close_step`].
    async fn record_vote_close_step(&self, vote_id: Uuid, step: VoteCloseStep) -> Result<()> {
        // Pick the statement setting the step provided, unless it was set already
        let statement = match step {
            VoteCloseStep::Announcement => {
                "
                update vote
                set close_announcement_posted_at = coalesce(close_announcement_posted_at, current_timestamp)
                where vote_id = $1::uuid
                "
            }
            VoteCloseStep::CheckRun => {
                "
                update vote
                set close_check_run_created_at = coalesce(close_check_run_created_at, current_timestamp)
                where vote_id = $1::uuid
                "
            }
            VoteCloseStep::Comment => {
                "
                update vote
                set close_comment_posted_at = coalesce(close_comment_posted_at, current_timestamp)
                where vote_id = $1::uuid
                "
            }
            VoteCloseStep::Labels => {
                "
                update vote
                set close_labels_updated_at = coalesce(close_labels_updated_at, current_timestamp)
                where vote_id = $1::uuid
                "
            }
        };

        // Record the step
        let db = self.pool.get().await?;
        db.execute(statement, &[&vote_id]).await?;
        Ok(())
    }

    /// [`DB::store_vote`].
    async fn store_vote(
        &self,
        vote_comment_id: i64,
        input: &CreateVoteInput,
        cfg: &CfgProfile,
    ) -> Result<Uuid> {
        // Insert the vote unless the issue/pr has one open already. In the same
        // statement, mark the check run and labels steps still pending of older votes in
        // the issue/pr as done, as the new vote supersedes the state they would publish
        // even if it is cancelled and deleted later (newer votes are only seen while
        // they exist). Their comment and announcement are still published
        let db = self.pool.get().await?;
        let row = db
            .query_opt(
                "
                with inserted as (
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
                        issue_title,
                        organization
                    ) values (
                        $1::jsonb,
                        $2::text,
                        current_timestamp + ($3::bigint || ' seconds')::interval,
                        $4::bigint,
                        $5::boolean,
                        $6::bigint,
                        $7::bigint,
                        $8::text,
                        $9::bigint,
                        $10::text,
                        $11::text
                    )
                    on conflict (repository_full_name, issue_number) where closed = false do nothing
                    returning vote_id, issue_number, repository_full_name
                ), superseded as (
                    update vote v
                    set
                        close_check_run_created_at = coalesce(
                            v.close_check_run_created_at,
                            current_timestamp
                        ),
                        close_labels_updated_at = coalesce(
                            v.close_labels_updated_at,
                            current_timestamp
                        )
                    from inserted i
                    where v.repository_full_name = i.repository_full_name
                    and v.issue_number = i.issue_number
                    and v.vote_id <> i.vote_id
                    and v.close_pending
                )
                select vote_id
                from inserted
                ",
                &[
                    &Json(&cfg),
                    &input.created_by,
                    &(cfg.duration.as_secs() as i64),
                    &input.installation_id,
                    &input.is_pull_request,
                    &input.issue_id,
                    &input.issue_number,
                    &input.repository_full_name,
                    &vote_comment_id,
                    &input.issue_title,
                    &input.organization,
                ],
            )
            .await?;

        // No row is returned when the insert was skipped
        let Some(row) = row else {
            return Err(VoteAlreadyOpenError.into());
        };
        Ok(row.get("vote_id"))
    }

    /// [`DB::update_vote_ends_at`].
    async fn update_vote_ends_at(&self, vote_id: Uuid) -> Result<()> {
        // Move the end of the vote to now, unless it has finished already
        let db = self.pool.get().await?;
        db.execute(
            "
            update vote
            set ends_at = current_timestamp
            where vote_id = $1::uuid
            and ends_at > current_timestamp;
            ",
            &[&vote_id],
        )
        .await?;
        Ok(())
    }

    /// [`DB::update_vote_last_check`].
    async fn update_vote_last_check(&self, vote_id: Uuid) -> Result<()> {
        let db = self.pool.get().await?;
        db.execute(
            "
            update vote
            set checked_at = current_timestamp
            where vote_id = $1::uuid;
            ",
            &[&vote_id],
        )
        .await?;
        Ok(())
    }
}

// Helpers.

/// Records a failed attempt to close the vote provided using the database client given
/// (see [`DB::record_vote_close_failure`]).
async fn record_vote_close_failure_with(
    db: &impl GenericClient,
    vote_id: Uuid,
    claimed_until: OffsetDateTime,
) -> Result<VoteCloseFailureOutcome> {
    // Provided that the caller still holds the claim, abandon the cancellation of a
    // cancelled vote on its last attempt deleting the vote, or schedule the next attempt
    // otherwise. Both statements see the same row, and their conditions exclude each
    // other, so only one of them applies. The exponent of the delay is capped, as votes
    // still open have no attempts limit
    let row = db
        .query_opt(
            "
            with abandoned as (
                delete from vote
                where vote_id = $1::uuid
                and close_next_attempt_at = $2::timestamptz
                and cancelled_at is not null
                and close_attempts + 1 >= $3::integer
                returning vote_id
            ), rescheduled as (
                update vote
                set
                    close_attempts = close_attempts + 1,
                    close_next_attempt_at = current_timestamp + make_interval(
                        secs => least(
                            $4::double precision * 2 ^ least(close_attempts, $5::integer),
                            $6::double precision
                        )
                    )
                where vote_id = $1::uuid
                and close_next_attempt_at = $2::timestamptz
                and not (cancelled_at is not null and close_attempts + 1 >= $3::integer)
                returning
                    close_attempts,
                    closed
            )
            select
                false as has_attempts_left,
                true as is_abandoned
            from abandoned
            union all
            select
                (not closed or close_attempts < $3::integer),
                false
            from rescheduled
            ",
            &[
                &vote_id,
                &claimed_until,
                &CLOSE_MAX_ATTEMPTS,
                &CLOSE_RETRY_BASE_DELAY_SECS,
                &CLOSE_RETRY_MAX_EXPONENT,
                &CLOSE_RETRY_MAX_DELAY_SECS,
            ],
        )
        .await?;

    // Tell the caller whether the failure was recorded and if more attempts are left
    let Some(row) = row else {
        return Ok(VoteCloseFailureOutcome::ClaimLost);
    };
    let outcome = if row.try_get("is_abandoned")? {
        VoteCloseFailureOutcome::CancellationAbandoned
    } else if row.try_get("has_attempts_left")? {
        VoteCloseFailureOutcome::RetryScheduled
    } else {
        VoteCloseFailureOutcome::NoAttemptsLeft
    };
    Ok(outcome)
}
