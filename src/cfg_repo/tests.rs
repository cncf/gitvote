use std::sync::Arc;

use futures::future;
use mockall::predicate::eq;
use serde_json::json;

use crate::github::MockGH;
use crate::testutil::*;

use super::*;

#[test]
fn automation_rule_does_not_match() {
    // Setup automation rule
    let rule = AutomationRule {
        patterns: vec!["path/image.svg".to_string()],
        profile: "default".to_string(),
    };
    // Check unrelated paths do not match
    assert!(
        !rule
            .matches(&[File {
                filename: "README.md".to_string()
            }])
            .unwrap()
    );
    assert!(
        !rule
            .matches(&[File {
                filename: "image.svg".to_string()
            }])
            .unwrap()
    );
}

#[test]
fn automation_rule_does_not_match_negated_pattern() {
    // Setup automation rule
    let rule = AutomationRule {
        patterns: vec!["*.md".to_string(), "!CHANGELOG.md".to_string()],
        profile: "default".to_string(),
    };

    // Check negated pattern prevents a match
    assert!(
        !rule
            .matches(&[File {
                filename: "CHANGELOG.md".to_string()
            }])
            .unwrap()
    );
}

#[test]
fn automation_rule_does_not_match_without_files() {
    // Setup automation rule
    let rule = AutomationRule {
        patterns: vec!["*".to_string()],
        profile: "default".to_string(),
    };

    // Check rule does not match without files
    assert!(!rule.matches(&[]).unwrap());
}

#[test]
fn automation_rule_invalid_pattern_returns_error() {
    // Setup automation rule
    let rule = AutomationRule {
        patterns: vec!["docs/{a,b".to_string()],
        profile: "default".to_string(),
    };

    // Check invalid pattern returns an error
    assert!(
        rule.matches(&[File {
            filename: "README.md".to_string()
        }])
        .is_err()
    );
}

#[test]
fn automation_rule_matches() {
    // Setup automation rule
    let rule = AutomationRule {
        patterns: vec!["*.md".to_string(), "file.txt".to_string()],
        profile: "default".to_string(),
    };
    // Check matching paths are accepted
    assert!(
        rule.matches(&[File {
            filename: "README.md".to_string()
        }])
        .unwrap()
    );
    assert!(
        rule.matches(&[File {
            filename: "path/file.txt".to_string()
        }])
        .unwrap()
    );
}

#[test]
fn automation_rule_matches_any_of_the_files() {
    // Setup automation rule
    let rule = AutomationRule {
        patterns: vec!["docs/**".to_string()],
        profile: "default".to_string(),
    };

    // Check any matching file is accepted
    assert!(
        rule.matches(&[
            File {
                filename: "src/main.rs".to_string()
            },
            File {
                filename: "docs/nested/guide.md".to_string()
            },
        ])
        .unwrap()
    );
}

#[test]
fn automation_rule_matches_anchored_pattern_only_from_root() {
    // Setup automation rule
    let rule = AutomationRule {
        patterns: vec!["/README.md".to_string()],
        profile: "default".to_string(),
    };

    // Check root path matches
    assert!(
        rule.matches(&[File {
            filename: "README.md".to_string()
        }])
        .unwrap()
    );
    // Check nested path does not match
    assert!(
        !rule
            .matches(&[File {
                filename: "docs/README.md".to_string()
            }])
            .unwrap()
    );
}

#[test]
fn cfg_profile_deserialize_pass_threshold_base() {
    // Setup cases (YAML snippet, expected pass threshold base)
    let cases = [
        ("", None),
        (
            "pass_threshold_base: allowed_voters",
            Some(PassThresholdBase::AllowedVoters),
        ),
        (
            "pass_threshold_base:\n  votes_cast: {}",
            Some(PassThresholdBase::VotesCast {
                exclude_abstentions: None,
            }),
        ),
        (
            "pass_threshold_base:\n  votes_cast:",
            Some(PassThresholdBase::VotesCast {
                exclude_abstentions: None,
            }),
        ),
        (
            "pass_threshold_base:\n  votes_cast:\n    exclude_abstentions: true",
            Some(PassThresholdBase::VotesCast {
                exclude_abstentions: Some(true),
            }),
        ),
        (
            "pass_threshold_base:\n  votes_cast:\n    exclude_abstentions: false",
            Some(PassThresholdBase::VotesCast {
                exclude_abstentions: Some(false),
            }),
        ),
    ];

    for (snippet, expected) in cases {
        // Check the profile parses with the expected pass threshold base
        let yaml = format!("duration: 5m\npass_threshold: 50\n{snippet}\n");
        let cfg: CfgProfile = serde_yaml::from_str(&yaml).unwrap_or_else(|err| panic!("{snippet}: {err}"));
        assert_eq!(cfg.pass_threshold_base, expected, "snippet: {snippet}");
    }
}

#[test]
fn cfg_profile_deserialize_pass_threshold_base_invalid() {
    // Setup invalid YAML snippets
    let cases = [
        "pass_threshold_base: other",
        "pass_threshold_base: votes_cast",
        "pass_threshold_base:\n  allowed_voters: {}",
        "pass_threshold_base:\n  votes_cast:\n    exclude_abstentions: \"yes\"",
    ];

    for snippet in cases {
        // Check the profile is rejected
        let yaml = format!("duration: 5m\npass_threshold: 50\n{snippet}\n");
        assert!(
            serde_yaml::from_str::<CfgProfile>(&yaml).is_err(),
            "snippet: {snippet}"
        );
    }
}

#[test]
fn cfg_profile_deserialize_stored_json() {
    // Setup JSON as stored in the vote cfg column
    let stored = json!({
        "duration": "1month 2days 3h",
        "pass_threshold": 66.5,
        "allowed_voters": {
            "teams": [TEAM1],
            "users": [USER1],
            "exclude_team_maintainers": true
        },
        "announcements": {
            "discussions": {
                "category": DISCUSSIONS_CATEGORY
            }
        },
        "periodic_status_check": "1 week",
        "close_on_passing": true,
        "close_on_passing_min_wait": "2 days"
    });

    // Check it deserializes into the expected profile
    let cfg: CfgProfile = serde_json::from_value(stored).unwrap();
    assert_eq!(
        cfg,
        CfgProfile {
            duration: Duration::from_secs(2_630_016 + 2 * 86_400 + 3 * 3_600),
            pass_threshold: 66.5,
            allowed_voters: Some(AllowedVoters {
                teams: Some(vec![TEAM1.to_string()]),
                users: Some(vec![USER1.to_string()]),
                exclude_team_maintainers: Some(true),
            }),
            announcements: Some(Announcements {
                discussions: Some(DiscussionsAnnouncements {
                    category: DISCUSSIONS_CATEGORY.to_string(),
                }),
            }),
            pass_threshold_base: None,
            periodic_status_check: Some("1 week".to_string()),
            close_on_passing: Some(true),
            close_on_passing_min_wait: Some("2 days".to_string()),
        }
    );
}

#[test]
fn cfg_profile_deserialize_stored_json_minimal() {
    // Setup JSON as stored in the vote cfg column
    let stored = json!({
        "duration": "5m",
        "pass_threshold": 50.0
    });

    // Check it deserializes into the expected profile
    let cfg: CfgProfile = serde_json::from_value(stored).unwrap();
    assert_eq!(
        cfg,
        CfgProfile {
            duration: Duration::from_mins(5),
            pass_threshold: 50.0,
            ..Default::default()
        }
    );
}

#[test]
fn cfg_profile_deserialize_stored_json_pass_threshold_base() {
    // Setup cases (stored JSON value, expected pass threshold base)
    let cases = [
        (json!("allowed_voters"), PassThresholdBase::AllowedVoters),
        (
            json!({"votes_cast": {}}),
            PassThresholdBase::VotesCast {
                exclude_abstentions: None,
            },
        ),
        (
            json!({"votes_cast": {"exclude_abstentions": true}}),
            PassThresholdBase::VotesCast {
                exclude_abstentions: Some(true),
            },
        ),
    ];

    for (stored_base, expected) in cases {
        // Check the stored profile deserializes into the expected one
        let stored = json!({
            "duration": "5m",
            "pass_threshold": 50.0,
            "pass_threshold_base": stored_base.clone()
        });
        let cfg: CfgProfile = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(
            cfg,
            CfgProfile {
                duration: Duration::from_mins(5),
                pass_threshold: 50.0,
                pass_threshold_base: Some(expected),
                ..Default::default()
            },
            "stored: {stored_base}"
        );

        // Check it serializes back into the same JSON
        assert_eq!(
            serde_json::to_value(&cfg).unwrap(),
            stored,
            "stored: {stored_base}"
        );
    }
}

#[test]
fn cfg_profile_serialize_full() {
    // Setup profile with all fields set
    let cfg = CfgProfile {
        duration: Duration::from_hours(49),
        pass_threshold: 75.0,
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string()]),
            users: Some(vec![USER1.to_string()]),
            exclude_team_maintainers: Some(false),
        }),
        announcements: Some(Announcements {
            discussions: Some(DiscussionsAnnouncements {
                category: DISCUSSIONS_CATEGORY.to_string(),
            }),
        }),
        pass_threshold_base: Some(PassThresholdBase::VotesCast {
            exclude_abstentions: Some(true),
        }),
        periodic_status_check: Some("1 day".to_string()),
        close_on_passing: Some(true),
        close_on_passing_min_wait: Some("1 hour".to_string()),
    };

    // Check the JSON stored in the database (keys are used in SQL queries)
    assert_eq!(
        serde_json::to_value(&cfg).unwrap(),
        json!({
            "duration": "2days 1h",
            "pass_threshold": 75.0,
            "allowed_voters": {
                "teams": [TEAM1],
                "users": [USER1],
                "exclude_team_maintainers": false
            },
            "announcements": {
                "discussions": {
                    "category": DISCUSSIONS_CATEGORY
                }
            },
            "pass_threshold_base": {
                "votes_cast": {
                    "exclude_abstentions": true
                }
            },
            "periodic_status_check": "1 day",
            "close_on_passing": true,
            "close_on_passing_min_wait": "1 hour"
        })
    );
}

#[test]
fn cfg_profile_serialize_skips_unset_optional_fields() {
    // Setup profile with empty optional sections
    let cfg = CfgProfile {
        duration: Duration::from_mins(5),
        pass_threshold: 50.0,
        allowed_voters: Some(AllowedVoters::default()),
        announcements: Some(Announcements::default()),
        ..Default::default()
    };

    // Check unset optional fields are skipped
    assert_eq!(
        serde_json::to_value(&cfg).unwrap(),
        json!({
            "duration": "5m",
            "pass_threshold": 50.0,
            "allowed_voters": {},
            "announcements": {}
        })
    );
}

#[test]
fn cfg_profile_validate_invalid_pass_threshold() {
    for pass_threshold in [-10.0, 0.0, 100.01, f64::NAN] {
        // Setup profile with invalid pass threshold
        let cfg = CfgProfile {
            pass_threshold,
            ..Default::default()
        };

        // Check validation fails
        assert_eq!(
            cfg.validate(OWNER_IS_ORG).unwrap_err().to_string(),
            ERR_INVALID_PASS_THRESHOLD,
            "pass threshold: {pass_threshold}"
        );
    }
}

#[test]
fn cfg_profile_validate_owner_not_org_with_empty_teams() {
    // Setup profile with empty teams
    let cfg = CfgProfile {
        pass_threshold: 50.0,
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![]),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Check validation allows non-org owner
    assert!(cfg.validate(!OWNER_IS_ORG).is_ok());
}

#[test]
fn cfg_profile_validate_owner_not_org_with_users_only() {
    // Setup profile with users only
    let cfg = CfgProfile {
        pass_threshold: 50.0,
        allowed_voters: Some(AllowedVoters {
            users: Some(vec![USER1.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Check validation allows non-org owner
    assert!(cfg.validate(!OWNER_IS_ORG).is_ok());
}

#[test]
fn cfg_profile_validate_owner_org_with_teams() {
    // Setup profile with teams
    let cfg = CfgProfile {
        pass_threshold: 50.0,
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Check validation allows org owner
    assert!(cfg.validate(OWNER_IS_ORG).is_ok());
}

#[test]
fn cfg_profile_validate_valid_pass_threshold() {
    for pass_threshold in [0.01, 50.0, 100.0] {
        // Setup profile with valid pass threshold
        let cfg = CfgProfile {
            pass_threshold,
            ..Default::default()
        };

        // Check validation succeeds
        assert!(
            cfg.validate(OWNER_IS_ORG).is_ok(),
            "pass threshold: {pass_threshold}"
        );
    }
}

#[tokio::test]
async fn get_cfg_config_not_found() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(None)));

    // Run and check the error returned
    assert_eq!(
        Cfg::get(Arc::new(gh), INST_ID, OWNER, REPO).await.unwrap_err(),
        CfgError::ConfigNotFound
    );
}

#[tokio::test]
async fn get_cfg_full_config() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| {
            let config = r#"
audit:
  enabled: true
automation:
  enabled: true
  rules:
    - patterns: ["*.md"]
      profile: default
profiles:
  default:
    duration: 2w
    pass_threshold: 66.6
    allowed_voters:
      teams: [team1]
      users: [user1]
      exclude_team_maintainers: true
    announcements:
      discussions:
        category: announcements
    pass_threshold_base:
      votes_cast:
        exclude_abstentions: true
    periodic_status_check: 1 week
    close_on_passing: true
    close_on_passing_min_wait: 1 day
"#;
            Box::pin(future::ready(Some(config.to_string())))
        });

    // Run and check the configuration returned
    assert_eq!(
        Cfg::get(Arc::new(gh), INST_ID, OWNER, REPO).await.unwrap(),
        Cfg {
            profiles: HashMap::from([(
                "default".to_string(),
                CfgProfile {
                    duration: Duration::from_hours(14 * 24),
                    pass_threshold: 66.6,
                    allowed_voters: Some(AllowedVoters {
                        teams: Some(vec![TEAM1.to_string()]),
                        users: Some(vec![USER1.to_string()]),
                        exclude_team_maintainers: Some(true),
                    }),
                    announcements: Some(Announcements {
                        discussions: Some(DiscussionsAnnouncements {
                            category: DISCUSSIONS_CATEGORY.to_string(),
                        }),
                    }),
                    pass_threshold_base: Some(PassThresholdBase::VotesCast {
                        exclude_abstentions: Some(true),
                    }),
                    periodic_status_check: Some("1 week".to_string()),
                    close_on_passing: Some(true),
                    close_on_passing_min_wait: Some("1 day".to_string()),
                }
            )]),
            audit: Some(Audit { enabled: true }),
            automation: Some(Automation {
                enabled: true,
                rules: vec![AutomationRule {
                    patterns: vec!["*.md".to_string()],
                    profile: "default".to_string(),
                }],
            }),
        }
    );
}

#[tokio::test]
async fn get_cfg_invalid_config_missing_profiles() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some("audit:\n  enabled: true\n".to_string()))));

    // Run and check the error returned
    assert!(matches!(
        Cfg::get(Arc::new(gh), INST_ID, OWNER, REPO).await.unwrap_err(),
        CfgError::InvalidConfig(_)
    ));
}

#[tokio::test]
async fn get_cfg_profile_config_not_found() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(None)));
    let gh = Arc::new(gh);

    // Run and check the error returned
    assert_eq!(
        CfgProfile::get(gh, INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await.unwrap_err(),
        CfgError::ConfigNotFound
    );
}

#[tokio::test]
async fn get_cfg_profile_default() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
    let gh = Arc::new(gh);

    // Run and check the profile returned
    assert_eq!(
        CfgProfile::get(gh, INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await.unwrap(),
        CfgProfile {
            duration: Duration::from_mins(5),
            pass_threshold: 50.0,
            allowed_voters: Some(AllowedVoters::default()),
            ..Default::default()
        }
    );
}

#[tokio::test]
async fn get_cfg_profile_invalid_config_invalid_yaml() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_invalid_config()))));
    let gh = Arc::new(gh);

    // Run and check the error returned
    assert!(matches!(
        CfgProfile::get(
            gh,
            INST_ID,
            OWNER,
            OWNER_IS_ORG,
            REPO,
            Some(PROFILE_NAME.to_string())
        )
        .await
        .unwrap_err(),
        CfgError::InvalidConfig(_)
    ));
}

#[tokio::test]
async fn get_cfg_profile_invalid_config_pass_threshold_base() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| {
            let config = r"
profiles:
  default:
    duration: 5m
    pass_threshold: 50
    pass_threshold_base: other
";
            Box::pin(future::ready(Some(config.to_string())))
        });
    let gh = Arc::new(gh);

    // Run and check the error returned
    assert!(matches!(
        CfgProfile::get(gh, INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await.unwrap_err(),
        CfgError::InvalidConfig(_)
    ));
}

#[tokio::test]
async fn get_cfg_profile_invalid_config_teams_owner_not_org() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
    let gh = Arc::new(gh);

    // Run and check the error returned
    assert_eq!(
        CfgProfile::get(
            gh,
            INST_ID,
            OWNER,
            !OWNER_IS_ORG,
            REPO,
            Some(PROFILE_NAME.to_string())
        )
        .await
        .unwrap_err(),
        CfgError::InvalidConfig(ERR_TEAMS_NOT_ALLOWED.to_string())
    );
}

#[tokio::test]
async fn get_cfg_profile_pass_threshold_base_votes_cast() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| {
            let config = r"
profiles:
  default:
    duration: 5m
    pass_threshold: 50.01
    pass_threshold_base:
      votes_cast:
        exclude_abstentions: true
";
            Box::pin(future::ready(Some(config.to_string())))
        });
    let gh = Arc::new(gh);

    // Run and check the profile returned
    assert_eq!(
        CfgProfile::get(gh, INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await.unwrap(),
        CfgProfile {
            duration: Duration::from_mins(5),
            pass_threshold: 50.01,
            pass_threshold_base: Some(PassThresholdBase::VotesCast {
                exclude_abstentions: Some(true),
            }),
            ..Default::default()
        }
    );
}

#[tokio::test]
async fn get_cfg_profile_profile1() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
    let gh = Arc::new(gh);

    // Run and check the profile returned
    assert_eq!(
        CfgProfile::get(
            gh,
            INST_ID,
            OWNER,
            OWNER_IS_ORG,
            REPO,
            Some(PROFILE_NAME.to_string())
        )
        .await
        .unwrap(),
        CfgProfile {
            duration: Duration::from_mins(10),
            pass_threshold: 75.0,
            allowed_voters: Some(AllowedVoters {
                teams: Some(vec![TEAM1.to_string()]),
                users: Some(vec![USER1.to_string(), USER2.to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }
    );
}

#[tokio::test]
async fn get_cfg_profile_profile_not_found() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
    let gh = Arc::new(gh);

    // Run and check the error returned
    assert_eq!(
        CfgProfile::get(
            gh,
            INST_ID,
            OWNER,
            OWNER_IS_ORG,
            REPO,
            Some("profile9".to_string())
        )
        .await
        .unwrap_err(),
        CfgError::ProfileNotFound
    );
}
