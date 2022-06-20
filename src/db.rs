use crate::{
    github::IssueCommentEvent,
    votes::{CfgProfile, Vote, VoteResults},
};
use anyhow::Result;
use async_trait::async_trait;
use deadpool_postgres::{Pool, Transaction};
use std::sync::Arc;
use tokio_postgres::types::Json;
use uuid::Uuid;

/// Type alias to represent a DB trait object.
pub(crate) type DynDB = Arc<dyn DB + Send + Sync>;

/// Trait that defines some operations a DB implementation must support.
#[async_trait]
pub(crate) trait DB {
    /// Get any pending finished vote.
    async fn get_pending_finished_vote(&self, tx: &Transaction<'_>) -> Result<Option<Vote>>;

    /// Check if the issue/pr provided already has a vote open.
    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool>;

    /// Return a reference to the internal database pool.
    fn pool(&self) -> &Pool;

    /// Store the vote provided in the database.
    async fn store_vote(
        &self,
        vote_comment_id: i64,
        cfg: &CfgProfile,
        event: &IssueCommentEvent,
    ) -> Result<Uuid>;

    /// Store the vote results provided in the database.
    async fn store_vote_results(
        &self,
        tx: &Transaction<'_>,
        vote_id: Uuid,
        results: &VoteResults,
    ) -> Result<()>;
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
}

#[async_trait]
impl DB for PgDB {
    /// Get any pending finished vote.
    async fn get_pending_finished_vote(&self, tx: &Transaction<'_>) -> Result<Option<Vote>> {
        // Get pending finished vote from database (if any)
        let row = match tx
            .query_opt(
                "
                select
                    vote_id,
                    vote_comment_id,
                    created_at,
                    created_by,
                    ends_at,
                    closed,
                    closed_at,
                    cfg,
                    installation_id,
                    issue_id,
                    issue_number,
                    is_pull_request,
                    repository_full_name,
                    organization,
                    results
                from vote
                where current_timestamp > ends_at and closed = false
                for update of vote skip locked
                limit 1
                ",
                &[],
            )
            .await?
        {
            Some(row) => row,
            None => return Ok(None),
        };

        // Prepare vote and return it
        let Json(cfg): Json<CfgProfile> = row.get("cfg");
        let results: Option<Json<VoteResults>> = row.get("results");
        let vote = Vote {
            vote_id: row.get("vote_id"),
            vote_comment_id: row.get("vote_comment_id"),
            created_at: row.get("created_at"),
            created_by: row.get("created_by"),
            ends_at: row.get("ends_at"),
            closed: row.get("closed"),
            closed_at: row.get("closed_at"),
            cfg,
            installation_id: row.get("installation_id"),
            issue_id: row.get("issue_id"),
            issue_number: row.get("issue_number"),
            is_pull_request: row.get("is_pull_request"),
            repository_full_name: row.get("repository_full_name"),
            organization: row.get("organization"),
            results: results.map(|Json(results)| results),
        };
        Ok(Some(vote))
    }

    /// Check if the issue/pr provided already has a vote open.
    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool> {
        let db = self.pool.get().await?;
        let row = db
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
            .await?;
        let vote_in_progress: bool = row.get(0);
        Ok(vote_in_progress)
    }

    /// Return a reference to the internal database pool.
    fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Store the vote provided in the database.
    async fn store_vote(
        &self,
        vote_comment_id: i64,
        cfg: &CfgProfile,
        event: &IssueCommentEvent,
    ) -> Result<Uuid> {
        let organization = event.organization.as_ref().map(|org| org.login.clone());
        let db = self.pool.get().await?;
        let row = db
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
                    &event.comment.user.login,
                    &event.installation.id,
                    &event.issue.id,
                    &event.issue.number,
                    &event.issue.pull_request.is_some(),
                    &event.repository.full_name,
                    &organization,
                ],
            )
            .await?;
        let vote_id: Uuid = row.get("vote_id");
        Ok(vote_id)
    }

    /// Store the vote results provided in the database.
    async fn store_vote_results(
        &self,
        tx: &Transaction<'_>,
        vote_id: Uuid,
        results: &VoteResults,
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