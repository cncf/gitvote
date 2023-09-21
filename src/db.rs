use crate::{
    cfg::CfgProfile,
    cmd::{CheckVoteInput, CreateVoteInput},
    github::{self, split_full_name, DynGH},
    results::{self, Vote, VoteResults},
};
use anyhow::Result;
use async_trait::async_trait;
use deadpool_postgres::{Pool, Transaction};
#[cfg(test)]
use mockall::automock;
use std::sync::Arc;
use tokio_postgres::types::Json;
use uuid::Uuid;

/// Type alias to represent a DB trait object.
pub(crate) type DynDB = Arc<dyn DB + Send + Sync>;

/// Trait that defines some operations a DB implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait DB {
    /// Cancel open vote (if exists) in the issue/pr provided.
    async fn cancel_vote(
        &self,
        repository_full_name: &str,
        issue_number: i64,
    ) -> Result<Option<Uuid>>;

    /// Close any pending finished vote.
    async fn close_finished_vote(&self, gh: DynGH) -> Result<Option<(Vote, Option<VoteResults>)>>;

    /// Get open vote (if available) in the issue/pr provided.
    async fn get_open_vote(
        &self,
        repository_full_name: &str,
        issue_number: i64,
    ) -> Result<Option<Vote>>;

    /// Get open votes that have close on passing enabled.
    async fn get_open_votes_with_close_on_passing(&self) -> Result<Vec<Vote>>;

    /// Get pending status checks.
    async fn get_pending_status_checks(&self) -> Result<Vec<CheckVoteInput>>;

    /// Check if the issue/pr provided has a vote.
    async fn has_vote(&self, repository_full_name: &str, issue_number: i64) -> Result<bool>;

    /// Check if the issue/pr provided already has a vote open.
    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool>;

    /// Store the vote provided in the database.
    async fn store_vote(
        &self,
        vote_comment_id: i64,
        input: &CreateVoteInput,
        cfg: &CfgProfile,
    ) -> Result<Uuid>;

    /// Update vote's ending timestamp.
    async fn update_vote_ends_at(&self, vote_id: Uuid) -> Result<()>;

    /// Update vote's last check ts.
    async fn update_vote_last_check(&self, vote_id: Uuid) -> Result<()>;
}

/// DB implementation backed by PostgreSQL.
pub(crate) struct PgDB {
    pool: Pool,
}

impl PgDB {
    /// Create a new PgDB instance.
    pub(crate) fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Get any pending finished vote.
    async fn get_pending_finished_vote(tx: &Transaction<'_>) -> Result<Option<Vote>> {
        let vote = tx
            .query_opt(
                "
                select *
                from vote
                where current_timestamp > ends_at
                and closed = false
                order by random()
                for update of vote skip locked
                limit 1
                ",
                &[],
            )
            .await?
            .map(|row| Vote::from(&row));
        Ok(vote)
    }

    /// Store the vote results provided in the database.
    async fn store_vote_results(
        tx: &Transaction<'_>,
        vote_id: Uuid,
        results: &Option<VoteResults>,
    ) -> Result<()> {
        tx.execute(
            "
            update vote set
                closed = true,
                closed_at = current_timestamp,
                results = $1::jsonb
            where vote_id = $2::uuid;
            ",
            &[&Json(&results), &vote_id],
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl DB for PgDB {
    /// [DB::cancel_vote]
    async fn cancel_vote(
        &self,
        repository_full_name: &str,
        issue_number: i64,
    ) -> Result<Option<Uuid>> {
        let db = self.pool.get().await?;
        let cancelled_vote_id = db
            .query_opt(
                "
                delete from vote
                where repository_full_name = $1::text
                and issue_number = $2::bigint
                and closed = false
                returning vote_id
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?
            .and_then(|row| row.get("vote_id"));
        Ok(cancelled_vote_id)
    }

    /// [DB::close_finished_vote]
    async fn close_finished_vote(&self, gh: DynGH) -> Result<Option<(Vote, Option<VoteResults>)>> {
        // Get pending finished vote (if any) from database
        let mut db = self.pool.get().await?;
        let tx = db.transaction().await?;
        let Some(vote) = PgDB::get_pending_finished_vote(&tx).await? else {
            return Ok(None);
        };

        // Calculate results
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let results = match results::calculate(gh, owner, repo, &vote).await {
            Ok(results) => Some(results),
            Err(err) => {
                if github::is_not_found_error(&err) {
                    // Vote comment was deleted. We still want to proceed and
                    // close the vote so that we don't try again to close it.
                    None
                } else {
                    return Err(err);
                }
            }
        };

        // Store results in database
        PgDB::store_vote_results(&tx, vote.vote_id, &results).await?;
        tx.commit().await?;

        Ok(Some((vote, results)))
    }

    /// [DB::get_open_vote]
    async fn get_open_vote(
        &self,
        repository_full_name: &str,
        issue_number: i64,
    ) -> Result<Option<Vote>> {
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
            .map(|row| Vote::from(&row));
        Ok(vote)
    }

    /// [DB::get_open_votes_with_close_on_passing]
    async fn get_open_votes_with_close_on_passing(&self) -> Result<Vec<Vote>> {
        let db = self.pool.get().await?;
        let votes = db
            .query(
                "
                select *
                from vote
                where closed = false
                and cfg ? 'close_on_passing'
                and (cfg->>'close_on_passing')::boolean = true
                ",
                &[],
            )
            .await?
            .iter()
            .map(Vote::from)
            .collect();
        Ok(votes)
    }

    /// [DB::get_pending_status_checks]
    async fn get_pending_status_checks(&self) -> Result<Vec<CheckVoteInput>> {
        let db = self.pool.get().await?;
        let inputs = db
            .query(
                "
                select repository_full_name, issue_number
                from vote
                where closed = false
                and cfg ? 'periodic_status_check'
                and string_to_interval(cfg->>'periodic_status_check') is not null
                and (cfg->>'periodic_status_check')::interval >= '1 day'::interval
                and current_timestamp > created_at + (cfg->>'periodic_status_check')::interval
                and
                    case when checked_at is not null then
                        current_timestamp > checked_at + (cfg->>'periodic_status_check')::interval
                    else true end
                and ends_at > current_timestamp + '1 hour'::interval
                ",
                &[],
            )
            .await?
            .iter()
            .map(|row| CheckVoteInput {
                repository_full_name: row.get("repository_full_name"),
                issue_number: row.get("issue_number"),
            })
            .collect();
        Ok(inputs)
    }

    /// [DB::has_vote]
    async fn has_vote(&self, repository_full_name: &str, issue_number: i64) -> Result<bool> {
        let db = self.pool.get().await?;
        let has_vote = db
            .query_one(
                "
                select exists (
                    select 1 from vote
                    where repository_full_name = $1::text
                    and issue_number = $2::bigint
                )
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?
            .get(0);
        Ok(has_vote)
    }

    /// [DB::has_vote_open]
    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool> {
        let db = self.pool.get().await?;
        let has_vote_open = db
            .query_one(
                "
                select exists (
                    select 1 from vote
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

    /// [DB::store_vote]
    async fn store_vote(
        &self,
        vote_comment_id: i64,
        input: &CreateVoteInput,
        cfg: &CfgProfile,
    ) -> Result<Uuid> {
        let db = self.pool.get().await?;
        let vote_id = db
            .query_one(
                "
                insert into vote (
                    vote_comment_id,
                    ends_at,
                    cfg,
                    created_by,
                    installation_id,
                    issue_id,
                    issue_number,
                    is_pull_request,
                    repository_full_name,
                    organization

                ) values (
                    $1::bigint,
                    current_timestamp + ($2::bigint || ' seconds')::interval,
                    $3::jsonb,
                    $4::text,
                    $5::bigint,
                    $6::bigint,
                    $7::bigint,
                    $8::boolean,
                    $9::text,
                    $10::text
                )
                returning vote_id
                ",
                &[
                    &vote_comment_id,
                    &(cfg.duration.as_secs() as i64),
                    &Json(&cfg),
                    &input.created_by,
                    &input.installation_id,
                    &input.issue_id,
                    &input.issue_number,
                    &input.is_pull_request,
                    &input.repository_full_name,
                    &input.organization,
                ],
            )
            .await?
            .get("vote_id");
        Ok(vote_id)
    }

    /// [DB::update_vote_ends_at]
    async fn update_vote_ends_at(&self, vote_id: Uuid) -> Result<()> {
        let db = self.pool.get().await?;
        db.execute(
            "
            update vote set
                ends_at = current_timestamp
            where vote_id = $1::uuid;
            ",
            &[&vote_id],
        )
        .await?;
        Ok(())
    }

    /// [DB::update_vote_last_check]
    async fn update_vote_last_check(&self, vote_id: Uuid) -> Result<()> {
        let db = self.pool.get().await?;
        db.execute(
            "
            update vote set
                checked_at = current_timestamp
            where vote_id = $1::uuid;
            ",
            &[&vote_id],
        )
        .await?;
        Ok(())
    }
}
