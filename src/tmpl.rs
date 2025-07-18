//! This module defines the templates used for the GitHub comments.

use askama::Template;

use crate::{
    cfg_repo::CfgProfile,
    cmd::CreateVoteInput,
    github::{TeamSlug, UserName},
    results::VoteResults,
};

/// Template for the config not found comment.
#[derive(Debug, Clone, Template)]
#[template(path = "config-not-found.md")]
pub(crate) struct ConfigNotFound {}

/// Template for the config profile not found comment.
#[derive(Debug, Clone, Template)]
#[template(path = "config-profile-not-found.md")]
pub(crate) struct ConfigProfileNotFound {}

/// Template for the index document.
#[derive(Debug, Clone, Template)]
#[template(path = "index.html")]
pub(crate) struct Index {}

/// Template for the invalid config comment.
#[derive(Debug, Clone, Template)]
#[template(path = "invalid-config.md")]
pub(crate) struct InvalidConfig<'a> {
    reason: &'a str,
}

impl<'a> InvalidConfig<'a> {
    /// Create a new `InvalidConfig` template.
    pub(crate) fn new(reason: &'a str) -> Self {
        Self { reason }
    }
}

/// Template for the no vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "no-vote-in-progress.md")]
pub(crate) struct NoVoteInProgress<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> NoVoteInProgress<'a> {
    /// Create a new `NoVoteInProgress` template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote cancelled comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-cancelled.md")]
pub(crate) struct VoteCancelled<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> VoteCancelled<'a> {
    /// Create a new `VoteCancelled` template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote checked recently comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-checked-recently.md")]
pub(crate) struct VoteCheckedRecently {}

/// Template for the vote closed comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed.md")]
pub(crate) struct VoteClosed<'a> {
    results: &'a VoteResults,
}

impl<'a> VoteClosed<'a> {
    /// Create a new `VoteClosed` template.
    pub(crate) fn new(results: &'a VoteResults) -> Self {
        Self { results }
    }
}

/// Template for the vote closed announcement.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed-announcement.md")]
pub(crate) struct VoteClosedAnnouncement<'a> {
    issue_number: i64,
    issue_title: &'a str,
    results: &'a VoteResults,
}

impl<'a> VoteClosedAnnouncement<'a> {
    /// Create a new `VoteClosedAnnouncement` template.
    pub(crate) fn new(issue_number: i64, issue_title: &'a str, results: &'a VoteResults) -> Self {
        Self {
            issue_number,
            issue_title,
            results,
        }
    }
}

/// Template for the vote created comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-created.md")]
pub(crate) struct VoteCreated<'a> {
    creator: &'a str,
    issue_title: &'a str,
    issue_number: i64,
    duration: String,
    pass_threshold: f64,
    org: &'a str,
    teams: &'a [TeamSlug],
    users: &'a [UserName],
}

impl<'a> VoteCreated<'a> {
    /// Create a new `VoteCreated` template.
    pub(crate) fn new(input: &'a CreateVoteInput, cfg: &'a CfgProfile) -> Self {
        // Prepare teams and users allowed to vote
        let (mut teams, mut users): (&[TeamSlug], &[UserName]) = (&[], &[]);
        if let Some(allowed_voters) = &cfg.allowed_voters {
            if let Some(v) = &allowed_voters.teams {
                teams = v.as_slice();
            }
            if let Some(v) = &allowed_voters.users {
                users = v.as_slice();
            }
        }

        // Get organization name if available
        let org = match &input.organization {
            Some(org) => org.as_ref(),
            None => "",
        };

        Self {
            creator: &input.created_by,
            issue_title: &input.issue_title,
            issue_number: input.issue_number,
            duration: humantime::format_duration(cfg.duration).to_string(),
            pass_threshold: cfg.pass_threshold,
            org,
            teams,
            users,
        }
    }
}

/// Template for the vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-in-progress.md")]
pub(crate) struct VoteInProgress<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> VoteInProgress<'a> {
    /// Create a new `VoteInProgress` template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote restricted comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-restricted.md")]
pub(crate) struct VoteRestricted<'a> {
    user: &'a str,
}

impl<'a> VoteRestricted<'a> {
    /// Create a new `VoteRestricted` template.
    pub(crate) fn new(user: &'a str) -> Self {
        Self { user }
    }
}

/// Template for the vote status comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-status.md")]
pub(crate) struct VoteStatus<'a> {
    results: &'a VoteResults,
}

impl<'a> VoteStatus<'a> {
    /// Create a new `VoteStatus` template.
    pub(crate) fn new(results: &'a VoteResults) -> Self {
        Self { results }
    }
}

mod filters {
    use std::collections::BTreeMap;

    use crate::{github::UserName, results::UserVote};

    /// Template filter that returns up to the requested number of non-binding
    /// votes from the votes collection provided sorted by timestamp (oldest
    /// first).
    #[allow(clippy::trivially_copy_pass_by_ref, clippy::unnecessary_wraps)]
    pub(crate) fn non_binding(
        votes: &BTreeMap<UserName, UserVote>,
        _: &dyn askama::Values,
        max: &i64,
    ) -> askama::Result<Vec<(UserName, UserVote)>> {
        let mut non_binding_votes: Vec<(UserName, UserVote)> =
            votes.iter().filter(|(_, v)| !v.binding).map(|(n, v)| (n.clone(), v.clone())).collect();
        non_binding_votes.sort_by(|a, b| a.1.timestamp.cmp(&b.1.timestamp));
        #[allow(clippy::cast_possible_truncation)]
        Ok(non_binding_votes.into_iter().take(*max as usize).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, env, fs};

    use askama::Template;
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};

    use crate::{
        cmd::CreateVoteInput,
        github::Event,
        results::{UserVote, VoteOption, VoteResults},
        testutil::*,
    };

    use super::*;

    fn golden_file_path(name: &str) -> String {
        format!("{TESTDATA_PATH}/templates/{name}.golden")
    }

    fn read_golden_file(name: &str) -> String {
        let path = golden_file_path(name);
        fs::read_to_string(&path).unwrap_or_else(|_| panic!("error reading golden file: {path}"))
    }

    fn write_golden_file(name: &str, content: &str) {
        let path = golden_file_path(name);
        fs::write(&path, content).expect("write golden file should succeed");
    }

    fn check_golden_file(name: &str, actual: &str) {
        if env::var("REGENERATE_GOLDEN_FILES").is_ok() {
            write_golden_file(name, actual);
        } else {
            let expected = read_golden_file(name);
            assert_eq!(actual, expected, "output does not match golden file ({name})");
        }
    }

    #[test]
    fn test_config_not_found() {
        let tmpl = ConfigNotFound {};
        let output = tmpl.render().unwrap();
        check_golden_file("config-not-found", &output);
    }

    #[test]
    fn test_config_profile_not_found() {
        let tmpl = ConfigProfileNotFound {};
        let output = tmpl.render().unwrap();
        check_golden_file("config-profile-not-found", &output);
    }

    #[test]
    fn test_vote_checked_recently() {
        let tmpl = VoteCheckedRecently {};
        let output = tmpl.render().unwrap();
        check_golden_file("vote-checked-recently", &output);
    }

    #[test]
    fn test_invalid_config() {
        let tmpl = InvalidConfig::new("Missing required field: pass_threshold");
        let output = tmpl.render().unwrap();
        check_golden_file("invalid-config", &output);
    }

    #[test]
    fn test_no_vote_in_progress_issue() {
        let tmpl = NoVoteInProgress::new("testuser", false);
        let output = tmpl.render().unwrap();
        check_golden_file("no-vote-in-progress-issue", &output);
    }

    #[test]
    fn test_no_vote_in_progress_pr() {
        let tmpl = NoVoteInProgress::new("testuser", true);
        let output = tmpl.render().unwrap();
        check_golden_file("no-vote-in-progress-pr", &output);
    }

    #[test]
    fn test_vote_cancelled_issue() {
        let tmpl = VoteCancelled::new("testuser", false);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-cancelled-issue", &output);
    }

    #[test]
    fn test_vote_cancelled_pr() {
        let tmpl = VoteCancelled::new("testuser", true);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-cancelled-pr", &output);
    }

    #[test]
    fn test_vote_in_progress_issue() {
        let tmpl = VoteInProgress::new("testuser", false);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-in-progress-issue", &output);
    }

    #[test]
    fn test_vote_in_progress_pr() {
        let tmpl = VoteInProgress::new("testuser", true);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-in-progress-pr", &output);
    }

    #[test]
    fn test_vote_restricted() {
        let tmpl = VoteRestricted::new("testuser");
        let output = tmpl.render().unwrap();
        check_golden_file("vote-restricted", &output);
    }

    #[test]
    fn test_vote_created_all_collaborators() {
        let event = Event::Issue(setup_test_issue_event());
        let input = CreateVoteInput::new(None, &event);
        let cfg = CfgProfile {
            duration: std::time::Duration::from_secs(86_400), // 1 day
            pass_threshold: 75.0,
            ..Default::default()
        };

        let tmpl = VoteCreated::new(&input, &cfg);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-created-all-collaborators", &output);
    }

    #[test]
    fn test_vote_created_with_teams_and_users() {
        let mut event = setup_test_issue_event();
        event.issue.title = "Add new feature X".to_string();
        event.issue.number = 42;
        let event = Event::Issue(event);
        let input = CreateVoteInput::new(None, &event);

        let cfg = CfgProfile {
            duration: std::time::Duration::from_secs(259_200), // 3 days
            pass_threshold: 51.0,
            allowed_voters: Some(crate::cfg_repo::AllowedVoters {
                teams: Some(vec!["core-team".into(), "maintainers".into()]),
                users: Some(vec!["alice".into(), "bob".into()]),
                exclude_team_maintainers: None,
            }),
            ..Default::default()
        };

        let tmpl = VoteCreated::new(&input, &cfg);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-created-with-teams-and-users", &output);
    }

    #[test]
    fn test_vote_closed_passed() {
        let mut votes = BTreeMap::new();
        votes.insert(
            "alice".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-01T10:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "bob".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-01T11:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "charlie".to_string(),
            UserVote {
                vote_option: VoteOption::Against,
                timestamp: OffsetDateTime::parse("2023-01-01T12:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "dave".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-01T13:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "eve".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-01T14:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "supporter1".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-01T15:00:00Z", &Rfc3339).unwrap(),
                binding: false,
            },
        );
        votes.insert(
            "supporter2".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-01T16:00:00Z", &Rfc3339).unwrap(),
                binding: false,
            },
        );

        let results = VoteResults {
            passed: true,
            in_favor_percentage: 80.0,
            pass_threshold: 50.0,
            in_favor: 4,
            against: 1,
            against_percentage: 20.0,
            abstain: 0,
            not_voted: 0,
            binding: 5,
            non_binding: 2,
            allowed_voters: 5,
            votes: votes.into_iter().collect(),
            pending_voters: vec![],
        };

        let tmpl = VoteClosed::new(&results);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-closed-passed", &output);
    }

    #[test]
    fn test_vote_closed_failed() {
        let mut votes = BTreeMap::new();
        votes.insert(
            "alice".to_string(),
            UserVote {
                vote_option: VoteOption::Against,
                timestamp: OffsetDateTime::parse("2023-01-02T10:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "bob".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-02T11:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "charlie".to_string(),
            UserVote {
                vote_option: VoteOption::Against,
                timestamp: OffsetDateTime::parse("2023-01-02T12:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "dave".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-02T13:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "eve".to_string(),
            UserVote {
                vote_option: VoteOption::Against,
                timestamp: OffsetDateTime::parse("2023-01-02T14:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );

        let results = VoteResults {
            passed: false,
            in_favor_percentage: 40.0,
            pass_threshold: 50.0,
            in_favor: 2,
            against: 3,
            against_percentage: 60.0,
            abstain: 0,
            not_voted: 0,
            binding: 5,
            non_binding: 0,
            allowed_voters: 5,
            votes: votes.into_iter().collect(),
            pending_voters: vec![],
        };

        let tmpl = VoteClosed::new(&results);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-closed-failed", &output);
    }

    #[test]
    fn test_vote_status_in_progress() {
        let mut votes = BTreeMap::new();
        votes.insert(
            "alice".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-03T10:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "bob".to_string(),
            UserVote {
                vote_option: VoteOption::Abstain,
                timestamp: OffsetDateTime::parse("2023-01-03T11:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "supporter".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-03T12:00:00Z", &Rfc3339).unwrap(),
                binding: false,
            },
        );

        let results = VoteResults {
            passed: false,
            in_favor_percentage: 33.33,
            pass_threshold: 50.0,
            in_favor: 1,
            against: 0,
            against_percentage: 0.0,
            abstain: 1,
            not_voted: 1,
            binding: 2,
            non_binding: 1,
            allowed_voters: 3,
            votes: votes.into_iter().collect(),
            pending_voters: vec!["charlie".to_string()],
        };

        let tmpl = VoteStatus::new(&results);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-status-in-progress", &output);
    }

    #[test]
    fn test_vote_closed_announcement() {
        let mut votes = BTreeMap::new();
        votes.insert(
            "alice".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-04T10:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "bob".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-04T11:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );
        votes.insert(
            "charlie".to_string(),
            UserVote {
                vote_option: VoteOption::Abstain,
                timestamp: OffsetDateTime::parse("2023-01-04T12:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );

        let results = VoteResults {
            passed: true,
            in_favor_percentage: 66.67,
            pass_threshold: 50.0,
            in_favor: 2,
            against: 0,
            against_percentage: 0.0,
            abstain: 1,
            not_voted: 0,
            binding: 3,
            non_binding: 0,
            allowed_voters: 3,
            votes: votes.into_iter().collect(),
            pending_voters: vec![],
        };

        let tmpl = VoteClosedAnnouncement::new(123, "Implement RFC-42", &results);
        let output = tmpl.render().unwrap();
        check_golden_file("vote-closed-announcement", &output);
    }

    #[test]
    fn test_non_binding_filter() {
        // Create a dummy struct that implements askama::Values
        struct DummyValues;
        impl askama::Values for DummyValues {
            fn get_value(&self, _: &str) -> Option<&(dyn std::any::Any + 'static)> {
                None
            }
        }

        let mut votes = BTreeMap::new();

        // Add some binding votes
        votes.insert(
            "alice".to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse("2023-01-05T10:00:00Z", &Rfc3339).unwrap(),
                binding: true,
            },
        );

        // Add non-binding votes with different timestamps
        for i in 0..5 {
            votes.insert(
                format!("supporter{i}"),
                UserVote {
                    vote_option: VoteOption::InFavor,
                    timestamp: OffsetDateTime::parse(&format!("2023-01-05T{:02}:00:00Z", 11 + i), &Rfc3339)
                        .unwrap(),
                    binding: false,
                },
            );
        }

        // Test with limit of 3
        let dummy_values = DummyValues;

        let filtered = filters::non_binding(&votes, &dummy_values, &3).unwrap();
        assert_eq!(filtered.len(), 3);

        // Verify they are sorted by timestamp
        assert_eq!(filtered[0].0, "supporter0");
        assert_eq!(filtered[1].0, "supporter1");
        assert_eq!(filtered[2].0, "supporter2");

        // Test with limit larger than available non-binding votes
        let filtered = filters::non_binding(&votes, &dummy_values, &10).unwrap();
        assert_eq!(filtered.len(), 5);
    }
}
