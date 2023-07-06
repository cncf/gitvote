use crate::{
    cfg::{AllowedVoters, CfgProfile},
    github::*,
    results::{UserVote, Vote, VoteOption, VoteResults},
};
use std::{collections::HashMap, fs, path::Path, time::Duration};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

pub(crate) const BRANCH: &str = "main";
pub(crate) const COMMENT_ID: i64 = 1234;
pub(crate) const ERROR: &str = "fake error";
pub(crate) const INST_ID: u64 = 1234;
pub(crate) const ISSUE_ID: i64 = 1234;
pub(crate) const ISSUE_NUM: i64 = 1;
pub(crate) const ORG: &str = "org";
pub(crate) const OWNER: &str = "owner";
pub(crate) const OWNER_IS_ORG: bool = true;
pub(crate) const PROFILE_NAME: &str = "profile1";
pub(crate) const REPO: &str = "repo";
pub(crate) const REPOFN: &str = "org/repo";
pub(crate) const TESTDATA_PATH: &str = "src/testdata";
pub(crate) const TITLE: &str = "Test title";
pub(crate) const USER: &str = "user";
pub(crate) const USER1: &str = "user1";
pub(crate) const USER2: &str = "user2";
pub(crate) const USER3: &str = "user3";
pub(crate) const USER4: &str = "user4";
pub(crate) const USER5: &str = "user5";
pub(crate) const TEAM1: &str = "team1";
pub(crate) const VOTE_ID: &str = "00000000-0000-0000-0000-000000000001";
pub(crate) const TIMESTAMP: &str = "2022-11-30T10:00:00Z";

pub(crate) fn get_test_invalid_config() -> String {
    fs::read_to_string(Path::new(TESTDATA_PATH).join("config-invalid.yml")).unwrap()
}

pub(crate) fn get_test_valid_config() -> String {
    fs::read_to_string(Path::new(TESTDATA_PATH).join("config.yml")).unwrap()
}

pub(crate) fn setup_test_issue_event() -> IssueEvent {
    IssueEvent {
        action: IssueEventAction::Other,
        installation: Installation { id: INST_ID as i64 },
        issue: Issue {
            id: ISSUE_ID,
            number: ISSUE_NUM,
            title: TITLE.to_string(),
            body: None,
            pull_request: None,
        },
        repository: Repository {
            full_name: REPOFN.to_string(),
        },
        organization: Some(Organization {
            login: ORG.to_string(),
        }),
        sender: User {
            login: USER.to_string(),
        },
    }
}

pub(crate) fn setup_test_issue_comment_event() -> IssueCommentEvent {
    IssueCommentEvent {
        action: IssueCommentEventAction::Other,
        comment: Comment {
            id: COMMENT_ID,
            body: None,
        },
        installation: Installation { id: INST_ID as i64 },
        issue: Issue {
            id: ISSUE_ID,
            number: ISSUE_NUM,
            title: TITLE.to_string(),
            body: None,
            pull_request: None,
        },
        repository: Repository {
            full_name: REPOFN.to_string(),
        },
        organization: Some(Organization {
            login: ORG.to_string(),
        }),
        sender: User {
            login: USER.to_string(),
        },
    }
}

pub(crate) fn setup_test_pr_event() -> PullRequestEvent {
    PullRequestEvent {
        action: PullRequestEventAction::Other,
        installation: Installation { id: INST_ID as i64 },
        pull_request: PullRequest {
            id: ISSUE_ID,
            number: ISSUE_NUM,
            title: TITLE.to_string(),
            body: None,
            base: PullRequestBase {
                reference: BRANCH.to_string(),
            },
        },
        repository: Repository {
            full_name: REPOFN.to_string(),
        },
        organization: Some(Organization {
            login: ORG.to_string(),
        }),
        sender: User {
            login: USER.to_string(),
        },
    }
}

pub(crate) fn setup_test_vote() -> Vote {
    Vote {
        vote_id: Uuid::parse_str(VOTE_ID).unwrap(),
        vote_comment_id: COMMENT_ID,
        created_at: OffsetDateTime::now_utc(),
        created_by: USER.to_string(),
        ends_at: OffsetDateTime::now_utc(),
        closed: false,
        closed_at: None,
        checked_at: None,
        cfg: CfgProfile {
            duration: Duration::from_secs(300),
            pass_threshold: 50.0,
            allowed_voters: Some(AllowedVoters {
                users: Some(vec![USER1.to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        },
        installation_id: INST_ID as i64,
        issue_id: ISSUE_ID,
        issue_number: ISSUE_NUM,
        is_pull_request: false,
        repository_full_name: REPOFN.to_string(),
        organization: Some(ORG.to_string()),
        results: None,
    }
}

pub(crate) fn setup_test_vote_results() -> VoteResults {
    VoteResults {
        passed: true,
        in_favor_percentage: 100.0,
        pass_threshold: 50.0,
        in_favor: 1,
        against: 0,
        abstain: 0,
        not_voted: 0,
        binding: 1,
        non_binding: 0,
        votes: HashMap::from([(
            USER1.to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                binding: true,
            },
        )]),
        allowed_voters: 1,
        pending_voters: vec![],
    }
}
