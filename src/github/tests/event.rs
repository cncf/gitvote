use std::{fs, mem::discriminant, path::Path};

use serde_json::json;

use crate::testutil::*;

use super::*;

#[test]
fn event_try_from_errors() {
    // Setup cases (event header, expected error)
    let test_cases = [
        (Some("issues"), EventError::InvalidBody(String::new())),
        (None, EventError::MissingHeader),
        (Some("push"), EventError::UnsupportedEvent),
    ];
    for (header, expected_err) in test_cases {
        // Parse event and check the error variant, as serde sets the body error detail
        let header = header.map(HeaderValue::from_static);
        let err = Event::try_from((header.as_ref(), b"{}".as_slice())).unwrap_err();
        assert_eq!(
            discriminant(&err),
            discriminant(&expected_err),
            "{header:?}: {err:?}"
        );
    }
}

#[test]
fn event_try_from_issue_comment_event() {
    // Setup event payload
    let header = HeaderValue::from_static("issue_comment");
    let body = fs::read(Path::new(TESTDATA_PATH).join("event-cmd.json")).unwrap();

    // Parse event and check the result
    let mut expected_event = setup_test_issue_comment_event();
    expected_event.action = IssueCommentEventAction::Created;
    expected_event.comment.body = Some("/vote".to_string());
    assert_eq!(
        Event::try_from((Some(&header), body.as_slice())).unwrap(),
        Event::IssueComment(expected_event)
    );
}

#[test]
fn event_try_from_issue_event() {
    // Setup event payload
    let header = HeaderValue::from_static("issues");
    let body = json!({
        "action": "opened",
        "installation": {"id": INST_ID},
        "issue": {
            "id": ISSUE_ID,
            "number": ISSUE_NUM,
            "title": TITLE,
            "body": "/vote",
            "pull_request": {"url": "https://api.github.com/repos/org/repo/pulls/1"}
        },
        "repository": {"full_name": REPOFN},
        "sender": {"login": USER}
    });

    // Parse event and check the result
    let mut expected_event = setup_test_issue_event();
    expected_event.action = IssueEventAction::Opened;
    expected_event.issue.body = Some("/vote".to_string());
    expected_event.issue.pull_request = Some(PullRequestInIssue {
        url: "https://api.github.com/repos/org/repo/pulls/1".to_string(),
    });
    expected_event.organization = None;
    assert_eq!(
        Event::try_from((Some(&header), body.to_string().as_bytes())).unwrap(),
        Event::Issue(expected_event)
    );
}

#[test]
fn event_try_from_pr_event() {
    // Setup event payload
    let header = HeaderValue::from_static("pull_request");
    let body = json!({
        "action": "synchronize",
        "installation": {"id": INST_ID},
        "pull_request": {
            "base": {"ref": BRANCH},
            "id": ISSUE_ID,
            "number": ISSUE_NUM,
            "title": TITLE,
            "body": null
        },
        "repository": {"full_name": REPOFN},
        "sender": {"login": USER},
        "organization": {"login": ORG}
    });

    // Parse event and check the result
    let mut expected_event = setup_test_pr_event();
    expected_event.action = PullRequestEventAction::Synchronize;
    assert_eq!(
        Event::try_from((Some(&header), body.to_string().as_bytes())).unwrap(),
        Event::PullRequest(expected_event)
    );
}

#[test]
fn event_try_from_unknown_action() {
    // Setup cases (event header, payload with an action not handled, expected event)
    let mut issue_event = setup_test_issue_event();
    let mut issue_body = serde_json::to_value(&issue_event).unwrap();
    issue_body["action"] = json!("edited");
    issue_event.action = IssueEventAction::Other;
    let mut pr_event = setup_test_pr_event();
    let mut pr_body = serde_json::to_value(&pr_event).unwrap();
    pr_body["action"] = json!("closed");
    pr_event.action = PullRequestEventAction::Other;
    let test_cases = [
        ("issues", issue_body, Event::Issue(issue_event)),
        ("pull_request", pr_body, Event::PullRequest(pr_event)),
    ];
    for (header, body, expected_event) in test_cases {
        // Parse event and check the action is taken as other
        let header = HeaderValue::from_static(header);
        assert_eq!(
            Event::try_from((Some(&header), body.to_string().as_bytes())).unwrap(),
            expected_event
        );
    }
}
