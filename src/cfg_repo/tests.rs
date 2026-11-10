use std::{collections::BTreeSet, sync::Arc};

use anyhow::format_err;
use futures::future;
use mockall::predicate::eq;
use serde_json::json;
use serde_yaml::{Mapping, Value};

use crate::{github::MockGH, testutil::*};

use super::*;

/// Configuration reference published for users.
const CONFIG_REFERENCE: &str = include_str!("../../docs/config/.gitvote.yml");

/// Periods that must be rejected by the validation.
const INVALID_PERIODS: &[&str] = &[
    "",
    " ",
    "5",
    "days",
    "0 days",
    "0 weeks",
    "-1 days",
    "+1 days",
    "1.5 days",
    "5 dayz",
    "5days",
    "5  days",
    " 5 days",
    "5 days ",
    "5 Days",
    "1 WEEK",
    "1 hour",
    "1 month",
    "1 week 2 days",
    "one week",
    "every 5 days",
    "٣ days",
    "36501 days",
    "5215 weeks",
    "18446744073709551615 weeks",
    "18446744073709551616 days",
];

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

    // Check invalid pattern returns an error naming it
    let err = rule
        .matches(&[File {
            filename: "README.md".to_string(),
        }])
        .unwrap_err();
    assert!(err.to_string().contains("docs/{a,b"), "error: {err}");
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
fn automation_rule_matches_directory_patterns() {
    // Setup cases (patterns, file, expected result)
    let cases = [
        (vec!["docs/"], "docs/guide.md", true),
        (vec!["docs"], "docs/guide.md", true),
        (vec!["/docs/"], "docs/guide.md", true),
        (vec!["docs/"], "docs/nested/deep/guide.md", true),
        (vec!["docs/"], "src/docs/guide.md", true),
        (vec![".github/"], ".github/workflows/ci.yml", true),
        (vec!["src/testdata"], "src/testdata/config.yml", true),
        (vec!["docs/**"], "docs/guide.md", true),
        (vec!["/docs/"], "src/docs/guide.md", false),
        (vec!["docs/"], "docs", false),
        (vec!["docs/"], "documentation/guide.md", false),
        (vec!["src/testdata"], "testdata/config.yml", false),
        (vec!["docs/", "!docs/keep.md"], "docs/keep.md", false),
        (vec!["docs/", "!docs/keep.md"], "docs/other.md", true),
    ];

    for (patterns, filename, expected) in cases {
        // Setup automation rule
        let rule = AutomationRule {
            patterns: patterns.iter().map(ToString::to_string).collect(),
            profile: "default".to_string(),
        };

        // Check the file is matched as expected
        let files = [File {
            filename: filename.to_string(),
        }];
        assert_eq!(
            rule.matches(&files).unwrap(),
            expected,
            "patterns: {patterns:?}, file: {filename}"
        );
    }
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
    // Setup cases (invalid YAML snippet, expected error fragment)
    let cases = [
        ("pass_threshold_base: other", "unknown variant `other`"),
        ("pass_threshold_base: votes_cast", "expected struct variant"),
        ("pass_threshold_base:\n  allowed_voters: {}", "invalid type: map"),
        (
            "pass_threshold_base:\n  votes_cast:\n    exclude_abstentions: \"yes\"",
            "invalid type: string \"yes\", expected a boolean",
        ),
    ];

    for (snippet, expected_error) in cases {
        // Check the profile is rejected for the expected reason
        let yaml = format!("duration: 5m\npass_threshold: 50\n{snippet}\n");
        let err = serde_yaml::from_str::<CfgProfile>(&yaml).unwrap_err().to_string();
        assert!(err.contains(expected_error), "snippet: {snippet}, error: {err}");
    }
}

#[test]
fn cfg_profile_deserialize_stored_json() {
    // Setup JSON as stored in the vote cfg column
    let stored = json!({
        "duration": "1month 2days 3h",
        "pass_threshold": 66.5,
        "allowed_voters": {
            "exclude_team_maintainers": true,
            "teams": [TEAM1],
            "users": [USER1]
        },
        "announcements": {
            "discussions": {
                "category": DISCUSSIONS_CATEGORY
            }
        },
        "close_on_passing": true,
        "close_on_passing_min_wait": "2 days",
        "periodic_status_check": "1 week"
    });

    // Check it deserializes into the expected profile
    let cfg: CfgProfile = serde_json::from_value(stored).unwrap();
    assert_eq!(
        cfg,
        CfgProfile {
            duration: Duration::from_secs(2_630_016 + 2 * 86_400 + 3 * 3_600),
            pass_threshold: 66.5,

            allowed_voters: Some(AllowedVoters {
                exclude_team_maintainers: Some(true),
                teams: Some(vec![TEAM1.to_string()]),
                users: Some(vec![USER1.to_string()]),
            }),
            announcements: Some(Announcements {
                discussions: Some(DiscussionsAnnouncements {
                    category: DISCUSSIONS_CATEGORY.to_string(),
                }),
            }),
            close_on_passing: Some(true),
            close_on_passing_min_wait: Some("2 days".to_string()),
            pass_threshold_base: None,
            periodic_status_check: Some("1 week".to_string()),
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
            exclude_team_maintainers: Some(false),
            teams: Some(vec![TEAM1.to_string()]),
            users: Some(vec![USER1.to_string()]),
        }),
        announcements: Some(Announcements {
            discussions: Some(DiscussionsAnnouncements {
                category: DISCUSSIONS_CATEGORY.to_string(),
            }),
        }),
        close_on_passing: Some(true),
        close_on_passing_min_wait: Some("1 hour".to_string()),
        pass_threshold_base: Some(PassThresholdBase::VotesCast {
            exclude_abstentions: Some(true),
        }),
        periodic_status_check: Some("1 day".to_string()),
    };

    // Check the JSON stored in the database (keys are used in SQL queries)
    assert_eq!(
        serde_json::to_value(&cfg).unwrap(),
        json!({
            "duration": "2days 1h",
            "pass_threshold": 75.0,
            "allowed_voters": {
                "exclude_team_maintainers": false,
                "teams": [TEAM1],
                "users": [USER1]
            },
            "announcements": {
                "discussions": {
                    "category": DISCUSSIONS_CATEGORY
                }
            },
            "close_on_passing": true,
            "close_on_passing_min_wait": "1 hour",
            "pass_threshold_base": {
                "votes_cast": {
                    "exclude_abstentions": true
                }
            },
            "periodic_status_check": "1 day"
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
fn cfg_profile_validate_duration_limit() {
    // Setup cases (duration, expected to be valid)
    let cases = [
        (Duration::ZERO, false),
        (Duration::from_nanos(1), false),
        (Duration::from_millis(999), false),
        (Duration::from_nanos(999_999_999), false),
        (MIN_DURATION, true),
        (Duration::from_millis(1500), true),
        (Duration::from_mins(1), true),
        (Duration::from_mins(5), true),
        (Duration::from_hours(12), true),
        (Duration::from_secs(36_500 * 86_400 - 1), true),
        (Duration::from_hours(36_500 * 24), true),
        (MAX_DURATION + Duration::from_nanos(1), false),
        (MAX_DURATION + Duration::from_secs(1), false),
        (Duration::from_hours(36_501 * 24), false),
        (Duration::from_secs(u64::MAX), false),
        (Duration::MAX, false),
    ];

    for (duration, expected_valid) in cases {
        // Setup profile with the duration provided
        let cfg = CfgProfile {
            duration,
            pass_threshold: 50.0,
            ..Default::default()
        };

        // Check the outcome of the validation
        let result = cfg.validate(OWNER_IS_ORG);
        if expected_valid {
            assert!(result.is_ok(), "duration: {duration:?}");
        } else {
            assert_eq!(
                result.unwrap_err().to_string(),
                ERR_INVALID_DURATION,
                "duration: {duration:?}"
            );
        }
    }
}

#[test]
fn cfg_profile_validate_invalid_pass_threshold() {
    for pass_threshold in [-10.0, 0.0, 100.01, f64::NAN] {
        // Setup profile with invalid pass threshold
        let cfg = CfgProfile {
            duration: Duration::from_mins(5),
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
fn cfg_profile_validate_invalid_periods() {
    for period in INVALID_PERIODS {
        // Setup profiles with an invalid period
        let period = Some((*period).to_string());
        let close_on_passing_cfg = CfgProfile {
            duration: Duration::from_mins(5),
            pass_threshold: 50.0,

            close_on_passing_min_wait: period.clone(),
            ..Default::default()
        };
        let periodic_status_check_cfg = CfgProfile {
            duration: Duration::from_mins(5),
            pass_threshold: 50.0,

            periodic_status_check: period.clone(),
            ..Default::default()
        };

        // Check validation fails
        assert_eq!(
            close_on_passing_cfg.validate(OWNER_IS_ORG).unwrap_err().to_string(),
            ERR_INVALID_CLOSE_ON_PASSING_MIN_WAIT,
            "period: {period:?}"
        );
        assert_eq!(
            periodic_status_check_cfg.validate(OWNER_IS_ORG).unwrap_err().to_string(),
            ERR_INVALID_PERIODIC_STATUS_CHECK,
            "period: {period:?}"
        );
    }
}

#[test]
fn cfg_profile_validate_owner_not_org_with_empty_teams() {
    // Setup profile with empty teams
    let cfg = CfgProfile {
        duration: Duration::from_mins(5),
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
        duration: Duration::from_mins(5),
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
        duration: Duration::from_mins(5),
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
            duration: Duration::from_mins(5),
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

#[test]
fn cfg_profile_validate_valid_periods() {
    for period in [
        "1 day",
        "1 days",
        "5 days",
        "007 days",
        "36500 days",
        "1 week",
        "1 weeks",
        "2 weeks",
        "5214 weeks",
    ] {
        // Setup profile with a valid period in both fields
        let cfg = CfgProfile {
            duration: Duration::from_mins(5),
            pass_threshold: 50.0,

            close_on_passing_min_wait: Some(period.to_string()),
            periodic_status_check: Some(period.to_string()),
            ..Default::default()
        };

        // Check validation succeeds
        assert!(cfg.validate(OWNER_IS_ORG).is_ok(), "period: {period}");
    }
}

#[test]
fn cfg_profile_validate_without_periods() {
    // Setup profile without periods
    let cfg = CfgProfile {
        duration: Duration::from_mins(5),
        pass_threshold: 50.0,
        ..Default::default()
    };

    // Check validation succeeds
    assert!(cfg.validate(OWNER_IS_ORG).is_ok());
}

#[test]
fn config_reference_active_values_parse_and_validate() {
    // Parse the configuration reference
    let cfg: Cfg = serde_yaml::from_str(CONFIG_REFERENCE).unwrap();

    // Check the default profile
    let default = &cfg.profiles["default"];
    assert_eq!(default.duration, Duration::from_mins(5));
    assert_eq!(default.pass_threshold, 50.0);
    assert_eq!(
        default.pass_threshold_base,
        Some(PassThresholdBase::AllowedVoters)
    );
    assert!(default.validate(OWNER_IS_ORG).is_ok());
    assert!(default.validate(!OWNER_IS_ORG).is_ok());

    // Check the additional profile, which uses teams
    let profile1 = &cfg.profiles[PROFILE_NAME];
    assert_eq!(profile1.duration, Duration::from_mins(1));
    assert_eq!(profile1.pass_threshold, 75.0);
    assert_eq!(
        profile1.allowed_voters.as_ref().and_then(|voters| voters.teams.clone()),
        Some(vec![TEAM1.to_string()])
    );
    assert!(profile1.validate(OWNER_IS_ORG).is_ok());
    assert_eq!(
        profile1.validate(!OWNER_IS_ORG).unwrap_err().to_string(),
        ERR_TEAMS_NOT_ALLOWED
    );

    // Check the automation rules use a profile defined in the file
    let automation = cfg.automation.as_ref().unwrap();
    assert!(cfg.profiles.contains_key(&automation.rules[0].profile));

    // Check no key in the file is ignored, as unknown keys are not reported
    let reference: Value = serde_yaml::from_str(CONFIG_REFERENCE).unwrap();
    assert_keys_are_read(&reference, &serde_yaml::to_value(&cfg).unwrap(), "");
}

#[test]
fn config_reference_examples_parse_and_validate() {
    // Extract the examples commented out in the configuration reference
    let examples = commented_examples(CONFIG_REFERENCE);

    // Check all the documented sections have examples
    let keys: BTreeSet<&str> = examples.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "allowed_voters",
            "announcements",
            "audit",
            "automation",
            "close_on_passing",
            "close_on_passing_min_wait",
            "pass_threshold",
            "pass_threshold_base",
            "periodic_status_check",
        ])
    );

    for (key, example) in &examples {
        // Setup a configuration using the example in the section it belongs to
        let mut profile: Mapping = serde_yaml::from_str("duration: 5m\npass_threshold: 50").unwrap();
        let mut cfg = Mapping::new();
        if ["audit", "automation"].contains(&key.as_str()) {
            cfg.insert(key.as_str().into(), example.clone());
        } else {
            profile.insert(key.as_str().into(), example.clone());
        }
        let mut profiles = Mapping::new();
        profiles.insert("default".into(), profile.into());
        cfg.insert("profiles".into(), profiles.into());
        let source = Value::Mapping(cfg);

        // Check the example parses and validates
        let cfg: Cfg = serde_yaml::from_value(source.clone())
            .unwrap_or_else(|err| panic!("key: {key}, example: {example:?}, error: {err}"));
        let validation = cfg.profiles["default"].validate(OWNER_IS_ORG);
        assert!(validation.is_ok(), "key: {key}, error: {validation:?}");

        // Check none of the keys in the example is ignored
        assert_keys_are_read(&source, &serde_yaml::to_value(&cfg).unwrap(), key);
    }
}

#[tokio::test]
async fn get_cfg_config_lookup_error() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the failure is not taken for a missing config file
    let err = Cfg::get(Arc::new(gh), INST_ID, OWNER, REPO).await.unwrap_err();
    assert!(matches!(&err, CfgError::ConfigLookup(source) if source.to_string() == ERROR));
}

#[tokio::test]
async fn get_cfg_config_not_found() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(None))));

    // Run and check the error returned
    assert!(matches!(
        Cfg::get(Arc::new(gh), INST_ID, OWNER, REPO).await.unwrap_err(),
        CfgError::ConfigNotFound
    ));
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
      exclude_team_maintainers: true
      teams: [team1]
      users: [user1]
    announcements:
      discussions:
        category: announcements
    close_on_passing: true
    close_on_passing_min_wait: 1 day
    pass_threshold_base:
      votes_cast:
        exclude_abstentions: true
    periodic_status_check: 1 week
"#;
            Box::pin(future::ready(Ok(Some(config.to_string()))))
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
                        exclude_team_maintainers: Some(true),
                        teams: Some(vec![TEAM1.to_string()]),
                        users: Some(vec![USER1.to_string()]),
                    }),
                    announcements: Some(Announcements {
                        discussions: Some(DiscussionsAnnouncements {
                            category: DISCUSSIONS_CATEGORY.to_string(),
                        }),
                    }),
                    close_on_passing: Some(true),
                    close_on_passing_min_wait: Some("1 day".to_string()),
                    pass_threshold_base: Some(PassThresholdBase::VotesCast {
                        exclude_abstentions: Some(true),
                    }),
                    periodic_status_check: Some("1 week".to_string()),
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
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some("audit:\n  enabled: true\n".to_string())))));

    // Run and check the reason in the error returned
    let err = Cfg::get(Arc::new(gh), INST_ID, OWNER, REPO).await.unwrap_err();
    assert!(
        matches!(&err, CfgError::InvalidConfig(reason) if reason.contains("missing field `profiles`")),
        "error: {err:?}"
    );
}

#[tokio::test]
async fn get_cfg_profile_config_not_found() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(None))));
    let gh = Arc::new(gh);

    // Run and check the error returned
    assert!(matches!(
        CfgProfile::get(gh, INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await.unwrap_err(),
        CfgError::ConfigNotFound
    ));
}

#[tokio::test]
async fn get_cfg_profile_default() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
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
async fn get_cfg_profile_default_with_teams_in_other_profile_owner_not_org() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));

    // Run and check only the profile requested is validated
    let profile = CfgProfile::get(Arc::new(gh), INST_ID, OWNER, !OWNER_IS_ORG, REPO, None).await.unwrap();
    assert_eq!(profile.duration, Duration::from_mins(5));
}

#[tokio::test]
async fn get_cfg_profile_duration_limit() {
    // Setup cases (duration, expected error fragment)
    let cases = [
        ("0", Some(ERR_INVALID_DURATION)),
        ("0s", Some(ERR_INVALID_DURATION)),
        ("0m", Some(ERR_INVALID_DURATION)),
        ("500ms", Some(ERR_INVALID_DURATION)),
        ("1s", None),
        ("1m", None),
        ("5m", None),
        ("12h", None),
        ("36500d", None),
        ("36500days 1s", Some(ERR_INVALID_DURATION)),
        ("36501d", Some(ERR_INVALID_DURATION)),
        ("100y", Some(ERR_INVALID_DURATION)),
        ("100000y", Some(ERR_INVALID_DURATION)),
        ("500000000000years", Some(ERR_INVALID_DURATION)),
        ("600000000000years", Some("profiles.default.duration")),
    ];

    for (duration, expected_error) in cases {
        // Setup GitHub expectations
        let mut gh = MockGH::new();
        let config = format!("profiles:\n  default:\n    duration: {duration}\n    pass_threshold: 50\n");
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(OWNER), eq(REPO))
            .times(1)
            .return_once(move |_, _, _| Box::pin(future::ready(Ok(Some(config)))));

        // Run and check the outcome
        let result = CfgProfile::get(Arc::new(gh), INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await;
        match expected_error {
            None => assert!(result.is_ok(), "duration: {duration}, result: {result:?}"),
            Some(expected_error) => assert!(
                matches!(&result, Err(CfgError::InvalidConfig(reason)) if reason.contains(expected_error)),
                "duration: {duration}, result: {result:?}"
            ),
        }
    }
}

#[tokio::test]
async fn get_cfg_profile_invalid_config_other_profile_invalid_duration() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_invalid_config())))));
    let gh = Arc::new(gh);

    // Run and check the whole file is rejected, naming the profile and the field
    let err = CfgProfile::get(
        gh,
        INST_ID,
        OWNER,
        OWNER_IS_ORG,
        REPO,
        Some(PROFILE_NAME.to_string()),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, CfgError::InvalidConfig(reason) if reason.contains("profiles.default.duration")),
        "error: {err:?}"
    );
}

#[tokio::test]
async fn get_cfg_profile_invalid_config_other_profile_missing_required_field() {
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
  other:
    duration: 10m
";
            Box::pin(future::ready(Ok(Some(config.to_string()))))
        });

    // Run and check the whole file is rejected, naming the profile and the field
    let err = CfgProfile::get(Arc::new(gh), INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await.unwrap_err();
    assert!(
        matches!(
            &err,
            CfgError::InvalidConfig(reason)
                if reason.contains("profiles.other") && reason.contains("missing field `pass_threshold`")
        ),
        "error: {err:?}"
    );
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
            Box::pin(future::ready(Ok(Some(config.to_string()))))
        });
    let gh = Arc::new(gh);

    // Run and check the reason in the error returned
    let err = CfgProfile::get(gh, INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await.unwrap_err();
    assert!(
        matches!(
            &err,
            CfgError::InvalidConfig(reason)
                if reason.contains("pass_threshold_base") && reason.contains("unknown variant `other`")
        ),
        "error: {err:?}"
    );
}

#[tokio::test]
async fn get_cfg_profile_invalid_config_periods() {
    // Setup cases (field, period, expected error)
    let cases = [
        (
            "close_on_passing_min_wait",
            "one week",
            ERR_INVALID_CLOSE_ON_PASSING_MIN_WAIT,
        ),
        (
            "periodic_status_check",
            "5 dayz",
            ERR_INVALID_PERIODIC_STATUS_CHECK,
        ),
        (
            "periodic_status_check",
            "every 5 days",
            ERR_INVALID_PERIODIC_STATUS_CHECK,
        ),
    ];

    for (field, period, expected_error) in cases {
        // Setup GitHub expectations
        let mut gh = MockGH::new();
        let config = format!(
            r#"
profiles:
  default:
    duration: 5m
    pass_threshold: 50
    {field}: "{period}"
"#
        );
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(OWNER), eq(REPO))
            .times(1)
            .return_once(move |_, _, _| Box::pin(future::ready(Ok(Some(config)))));

        // Run and check the reason in the error returned
        let err = CfgProfile::get(Arc::new(gh), INST_ID, OWNER, OWNER_IS_ORG, REPO, None).await.unwrap_err();
        assert!(
            matches!(&err, CfgError::InvalidConfig(reason) if reason == expected_error),
            "field: {field}, period: {period}, error: {err:?}"
        );
    }
}

#[tokio::test]
async fn get_cfg_profile_invalid_config_teams_owner_not_org() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
    let gh = Arc::new(gh);

    // Run and check the error returned
    assert!(matches!(
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
        CfgError::InvalidConfig(reason) if reason == ERR_TEAMS_NOT_ALLOWED
    ));
}

#[tokio::test]
async fn get_cfg_profile_only_validates_the_profile_requested() {
    // Setup cases (profile requested, expected to be valid)
    let cases = [(None, true), (Some("other"), false)];

    for (profile_name, expected_valid) in cases {
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
  other:
    duration: 5m
    pass_threshold: 0
";
                Box::pin(future::ready(Ok(Some(config.to_string()))))
            });

        // Run and check the outcome
        let profile_name = profile_name.map(ToString::to_string);
        let result = CfgProfile::get(
            Arc::new(gh),
            INST_ID,
            OWNER,
            OWNER_IS_ORG,
            REPO,
            profile_name.clone(),
        )
        .await;
        if expected_valid {
            assert!(result.is_ok(), "profile: {profile_name:?}, result: {result:?}");
        } else {
            assert!(
                matches!(&result, Err(CfgError::InvalidConfig(reason)) if reason == ERR_INVALID_PASS_THRESHOLD),
                "profile: {profile_name:?}, result: {result:?}"
            );
        }
    }
}

#[tokio::test]
async fn get_cfg_profile_other_profile_does_not_inherit_from_default() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(OWNER), eq(REPO))
        .times(1)
        .returning(|_, _, _| {
            let config = r#"
profiles:
  default:
    duration: 5m
    pass_threshold: 50
    allowed_voters:
      users:
        - user1
    announcements:
      discussions:
        category: announcements
    close_on_passing: true
    close_on_passing_min_wait: "1 week"
    pass_threshold_base:
      votes_cast: {}
    periodic_status_check: "5 days"
  other:
    duration: 10m
    pass_threshold: 75
"#;
            Box::pin(future::ready(Ok(Some(config.to_string()))))
        });

    // Run and check the optional fields are not taken from the default profile
    assert_eq!(
        CfgProfile::get(
            Arc::new(gh),
            INST_ID,
            OWNER,
            OWNER_IS_ORG,
            REPO,
            Some("other".to_string())
        )
        .await
        .unwrap(),
        CfgProfile {
            duration: Duration::from_mins(10),
            pass_threshold: 75.0,
            ..Default::default()
        }
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
            Box::pin(future::ready(Ok(Some(config.to_string()))))
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
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
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
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
    let gh = Arc::new(gh);

    // Run and check the error returned
    assert!(matches!(
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
    ));
}

// Helpers.

/// Checks all the keys in the source value with a value set are present in the parsed
/// one, as unknown keys would be ignored silently when parsing a configuration file.
fn assert_keys_are_read(source: &Value, parsed: &Value, path: &str) {
    match source {
        // Check each key with a value set was read, then check its value
        Value::Mapping(mapping) => {
            for (key, value) in mapping {
                let key_path = format!("{path}/{}", key.as_str().unwrap_or_default());
                if value.is_null() {
                    continue;
                }
                let parsed_value = parsed.get(key).unwrap_or_else(|| panic!("key not read: {key_path}"));
                assert_keys_are_read(value, parsed_value, &key_path);
            }
        }
        // Check each item was read, then check its value
        Value::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                let item_path = format!("{path}/{i}");
                let parsed_item = parsed.get(i).unwrap_or_else(|| panic!("item not read: {item_path}"));
                assert_keys_are_read(item, parsed_item, &item_path);
            }
        }
        // Scalar values have no keys to check
        _ => {}
    }
}

/// Extracts the YAML examples commented out in the configuration reference, returning
/// the key of each one along with its value. An example starts with a comment holding
/// a lowercase key, and goes on while the comments below it are indented.
fn commented_examples(reference: &str) -> Vec<(String, Value)> {
    // Collect the source of the examples
    let mut sources: Vec<String> = vec![];
    let mut in_example = false;
    for line in reference.lines() {
        let comment = line.trim_start().strip_prefix("# ").unwrap_or_default();

        // Append indented comments to the example in progress
        if in_example && comment.starts_with(' ') {
            let source = sources.last_mut().expect("example to be started");
            source.push('\n');
            source.push_str(comment);
            continue;
        }

        // Start a new example when the comment holds a lowercase key
        let key = comment.split_once(':').map_or("", |(key, _)| key);
        in_example = !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_');
        if in_example {
            sources.push(comment.to_string());
        }
    }

    // Parse each example into its key and value
    sources
        .iter()
        .map(|source| {
            let example: Mapping =
                serde_yaml::from_str(source).unwrap_or_else(|err| panic!("example: {source}, error: {err}"));
            let (key, value) = example.into_iter().next().expect("example to have a key");
            (key.as_str().expect("key to be a string").to_string(), value)
        })
        .collect()
}
