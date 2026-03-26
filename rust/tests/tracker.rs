use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use symphony_rust::tracker::linear::{GraphqlRequest, LinearTracker, MockLinearGraphqlExecutor};
use symphony_rust::tracker::memory::MemoryTracker;
use symphony_rust::tracker::{
    AssigneeMode, LinearTrackerConfig, Tracker, TrackerError, TrackerEvent,
};
use symphony_rust::types::{BlockerRef, Issue, IssueId, IssueIdentifier};

fn issue(id: &str, identifier: &str, state: &str) -> Issue {
    Issue {
        id: IssueId::new(id),
        identifier: IssueIdentifier::new(identifier),
        title: format!("Issue {identifier}"),
        description: Some("description".to_owned()),
        priority: Some(2),
        state: state.to_owned(),
        branch_name: Some(format!("feature/{identifier}")),
        url: Some(format!("https://linear.app/example/{identifier}")),
        labels: vec!["module".to_owned()],
        blocked_by: vec![BlockerRef {
            id: Some(IssueId::new("dep-1")),
            identifier: Some(IssueIdentifier::new("SPA-0")),
            state: Some("Done".to_owned()),
        }],
        created_at: Some(parse_datetime("2026-03-26T10:00:00Z")),
        updated_at: Some(parse_datetime("2026-03-26T11:00:00Z")),
    }
}

fn parse_datetime(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("test datetime should parse")
        .with_timezone(&Utc)
}

fn linear_config() -> LinearTrackerConfig {
    LinearTrackerConfig {
        api_key: Some("linear-token".to_owned()),
        project_slug: Some("symphony".to_owned()),
        ..LinearTrackerConfig::default()
    }
}

#[tokio::test]
async fn memory_tracker_filters_states_and_records_write_events() {
    let tracker = MemoryTracker::new(vec![
        issue("1", "SPA-1", "Todo"),
        issue("2", "SPA-2", "Done"),
        issue("3", "SPA-3", "In Progress"),
    ]);

    let by_state = tracker
        .fetch_issues_by_states(&["todo".to_owned(), "in progress".to_owned()])
        .await
        .expect("memory tracker should filter issues");
    let by_id = tracker
        .fetch_issue_states_by_ids(&["3".to_owned(), "1".to_owned()])
        .await
        .expect("memory tracker should fetch by id");

    tracker
        .create_comment("1", "hello")
        .await
        .expect("comment should be recorded");
    tracker
        .update_issue_state("3", "Done")
        .await
        .expect("state update should be recorded");

    assert_eq!(by_state.len(), 2);
    assert_eq!(
        by_id
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>(),
        vec!["3", "1"]
    );
    assert_eq!(
        tracker.recorded_events(),
        vec![
            TrackerEvent::CommentCreated {
                issue_id: "1".to_owned(),
                body: "hello".to_owned(),
            },
            TrackerEvent::IssueStateUpdated {
                issue_id: "3".to_owned(),
                state_name: "Done".to_owned(),
            },
        ]
    );
}

#[tokio::test]
async fn candidate_fetch_uses_project_slug_pagination_and_normalization_rules() {
    let requests = Arc::new(Mutex::new(Vec::<GraphqlRequest>::new()));
    let responses = Arc::new(Mutex::new(VecDeque::from([
        Ok(json!({
            "data": {
                "issues": {
                    "nodes": [
                        {
                            "id": "issue-1",
                            "identifier": "SPA-11",
                            "title": "Tracker client",
                            "description": "Implement tracker",
                            "priority": 1.0,
                            "state": { "name": "Todo" },
                            "branchName": "feature/SPA-11",
                            "url": "https://linear.app/issue/SPA-11",
                            "assignee": { "id": "viewer-1" },
                            "labels": { "nodes": [{ "name": "Rust" }, { "name": "Module-4" }] },
                            "inverseRelations": {
                                "nodes": [
                                    {
                                        "type": "blocks",
                                        "issue": {
                                            "id": "dep-1",
                                            "identifier": "SPA-10",
                                            "state": { "name": "Done" }
                                        }
                                    },
                                    {
                                        "type": "relates to",
                                        "issue": {
                                            "id": "dep-2",
                                            "identifier": "SPA-9",
                                            "state": { "name": "Todo" }
                                        }
                                    }
                                ]
                            },
                            "createdAt": "2026-03-26T10:00:00Z",
                            "updatedAt": "2026-03-26T11:00:00Z"
                        }
                    ],
                    "pageInfo": { "hasNextPage": true, "endCursor": "cursor-1" }
                }
            }
        })),
        Ok(json!({
            "data": {
                "issues": {
                    "nodes": [
                        {
                            "id": "issue-2",
                            "identifier": "SPA-12",
                            "title": "Priority normalization",
                            "description": null,
                            "priority": 1.5,
                            "state": { "name": "In Progress" },
                            "branchName": null,
                            "url": null,
                            "assignee": { "id": "viewer-1" },
                            "labels": { "nodes": [{ "name": "Bug" }] },
                            "inverseRelations": { "nodes": [] },
                            "createdAt": "2026-03-26T12:00:00Z",
                            "updatedAt": "2026-03-26T13:00:00Z"
                        }
                    ],
                    "pageInfo": { "hasNextPage": false, "endCursor": null }
                }
            }
        })),
    ])));

    let tracker = LinearTracker::with_executor(
        LinearTrackerConfig {
            assignee: AssigneeMode::Id("viewer-1".to_owned()),
            ..linear_config()
        },
        MockLinearGraphqlExecutor::from_responses(requests.clone(), responses),
    );

    let issues = tracker
        .fetch_candidate_issues()
        .await
        .expect("candidate fetch should succeed");

    let requests = requests
        .lock()
        .expect("requests mutex should not be poisoned");
    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].id.as_str(), "issue-1");
    assert_eq!(issues[0].labels, vec!["rust", "module-4"]);
    assert_eq!(issues[0].blocked_by.len(), 1);
    assert_eq!(
        issues[0].blocked_by[0]
            .identifier
            .as_ref()
            .map(|id| id.as_str()),
        Some("SPA-10")
    );
    assert_eq!(issues[1].priority, None);
    assert_eq!(requests.len(), 2);
    assert!(requests[0]
        .query
        .contains("project: {slugId: {eq: $projectSlug}}"));
    assert_eq!(requests[0].variables["projectSlug"], "symphony");
    assert_eq!(requests[0].variables["first"], 50);
    assert!(requests[0].variables["after"].is_null());
    assert_eq!(requests[1].variables["after"], "cursor-1");
}

#[tokio::test]
async fn fetch_issue_states_by_ids_batches_and_preserves_request_order() {
    let requests = Arc::new(Mutex::new(Vec::<GraphqlRequest>::new()));
    let mut batch_1_nodes = Vec::new();
    let mut batch_2_nodes = Vec::new();

    for index in 0..50 {
        batch_1_nodes.push(issue_node(
            &format!("id-{index}"),
            &format!("SPA-{index}"),
            "Todo",
        ));
    }

    for index in 50..55 {
        batch_2_nodes.push(issue_node(
            &format!("id-{index}"),
            &format!("SPA-{index}"),
            "In Progress",
        ));
    }

    let tracker = LinearTracker::with_executor(
        linear_config(),
        MockLinearGraphqlExecutor::from_responses(
            requests.clone(),
            Arc::new(Mutex::new(VecDeque::from([
                Ok(
                    json!({ "data": { "issues": { "nodes": batch_1_nodes.into_iter().rev().collect::<Vec<_>>() } } }),
                ),
                Ok(
                    json!({ "data": { "issues": { "nodes": batch_2_nodes.into_iter().rev().collect::<Vec<_>>() } } }),
                ),
            ]))),
        ),
    );

    let requested_ids = (0..55)
        .rev()
        .map(|index| format!("id-{index}"))
        .collect::<Vec<_>>();
    let issues = tracker
        .fetch_issue_states_by_ids(&requested_ids)
        .await
        .expect("issue-state refresh should succeed");

    let requests = requests
        .lock()
        .expect("requests mutex should not be poisoned");
    assert_eq!(issues.len(), 55);
    assert_eq!(issues.first().map(|issue| issue.id.as_str()), Some("id-54"));
    assert_eq!(issues.last().map(|issue| issue.id.as_str()), Some("id-0"));
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].variables["ids"].as_array().map(Vec::len),
        Some(50)
    );
    assert_eq!(
        requests[1].variables["ids"].as_array().map(Vec::len),
        Some(5)
    );
}

#[tokio::test]
async fn fetch_issue_states_by_ids_keeps_reassigned_issues_in_results() {
    let requests = Arc::new(Mutex::new(Vec::<GraphqlRequest>::new()));
    let tracker = LinearTracker::with_executor(
        LinearTrackerConfig {
            assignee: AssigneeMode::Id("viewer-1".to_owned()),
            ..linear_config()
        },
        MockLinearGraphqlExecutor::from_responses(
            requests,
            Arc::new(Mutex::new(VecDeque::from([Ok(json!({
                "data": {
                    "issues": {
                        "nodes": [issue_node_with_assignee("id-1", "SPA-1", "In Progress", "someone-else")]
                    }
                }
            }))]))),
        ),
    );

    let issues = tracker
        .fetch_issue_states_by_ids(&["id-1".to_owned()])
        .await
        .expect("issue-state refresh should succeed even when reassigned");

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].id.as_str(), "id-1");
}

#[tokio::test]
async fn fetch_issues_by_states_skips_api_for_empty_state_filters() {
    let requests = Arc::new(Mutex::new(Vec::<GraphqlRequest>::new()));
    let tracker = LinearTracker::with_executor(
        linear_config(),
        MockLinearGraphqlExecutor::from_responses(
            requests.clone(),
            Arc::new(Mutex::new(VecDeque::new())),
        ),
    );

    let issues = tracker
        .fetch_issues_by_states(&[])
        .await
        .expect("empty states should short-circuit");

    assert!(issues.is_empty());
    assert!(requests
        .lock()
        .expect("requests mutex should not be poisoned")
        .is_empty());
}

#[tokio::test]
async fn maps_linear_error_categories() {
    let missing_key = LinearTracker::new(LinearTrackerConfig {
        api_key: None,
        ..linear_config()
    })
    .expect("tracker construction should succeed");
    let missing_slug = LinearTracker::new(LinearTrackerConfig {
        project_slug: None,
        ..linear_config()
    })
    .expect("tracker construction should succeed");

    let status_tracker = LinearTracker::with_executor(
        linear_config(),
        MockLinearGraphqlExecutor::from_responses(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(VecDeque::from([Err(
                TrackerError::LinearApiStatus(503),
            )]))),
        ),
    );
    let graphql_tracker = LinearTracker::with_executor(
        linear_config(),
        MockLinearGraphqlExecutor::from_responses(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(VecDeque::from([Ok(json!({
                "errors": [{ "message": "boom" }]
            }))]))),
        ),
    );
    let request_tracker = LinearTracker::with_executor(
        linear_config(),
        MockLinearGraphqlExecutor::from_responses(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(VecDeque::from([Err(
                TrackerError::LinearApiRequest("network".to_owned()),
            )]))),
        ),
    );

    assert_eq!(
        missing_key.fetch_candidate_issues().await,
        Err(TrackerError::MissingApiKey)
    );
    assert_eq!(
        missing_slug.fetch_candidate_issues().await,
        Err(TrackerError::MissingProjectSlug)
    );
    assert_eq!(
        status_tracker.fetch_candidate_issues().await,
        Err(TrackerError::LinearApiStatus(503))
    );
    assert_eq!(
        graphql_tracker.fetch_candidate_issues().await,
        Err(TrackerError::LinearGraphqlErrors(vec!["boom".to_owned()]))
    );
    assert_eq!(
        request_tracker.fetch_candidate_issues().await,
        Err(TrackerError::LinearApiRequest("network".to_owned()))
    );
}

#[tokio::test]
async fn create_comment_and_update_issue_state_execute_expected_mutations() {
    let requests = Arc::new(Mutex::new(Vec::<GraphqlRequest>::new()));
    let tracker = LinearTracker::with_executor(
        linear_config(),
        MockLinearGraphqlExecutor::from_responses(
            requests.clone(),
            Arc::new(Mutex::new(VecDeque::from([
                Ok(json!({ "data": { "commentCreate": { "success": true } } })),
                Ok(
                    json!({ "data": { "issue": { "team": { "states": { "nodes": [{ "id": "state-1" }] } } } } }),
                ),
                Ok(json!({ "data": { "issueUpdate": { "success": true } } })),
            ]))),
        ),
    );

    tracker
        .create_comment("issue-1", "hello")
        .await
        .expect("comment mutation should succeed");
    tracker
        .update_issue_state("issue-1", "Done")
        .await
        .expect("issue update mutation should succeed");

    let requests = requests
        .lock()
        .expect("requests mutex should not be poisoned");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].query.contains("commentCreate"));
    assert_eq!(requests[0].variables["issueId"], "issue-1");
    assert_eq!(requests[0].variables["body"], "hello");
    assert!(requests[1].query.contains("SymphonyResolveStateId"));
    assert_eq!(requests[1].variables["stateName"], "Done");
    assert!(requests[2].query.contains("issueUpdate"));
    assert_eq!(requests[2].variables["stateId"], "state-1");
}

fn issue_node(id: &str, identifier: &str, state: &str) -> Value {
    json!({
        "id": id,
        "identifier": identifier,
        "title": identifier,
        "description": null,
        "priority": 2.0,
        "state": { "name": state },
        "branchName": null,
        "url": null,
        "assignee": null,
        "labels": { "nodes": [] },
        "inverseRelations": { "nodes": [] },
        "createdAt": "2026-03-26T12:00:00Z",
        "updatedAt": "2026-03-26T13:00:00Z"
    })
}

fn issue_node_with_assignee(id: &str, identifier: &str, state: &str, assignee_id: &str) -> Value {
    json!({
        "id": id,
        "identifier": identifier,
        "title": identifier,
        "description": null,
        "priority": 2.0,
        "state": { "name": state },
        "branchName": null,
        "url": null,
        "assignee": { "id": assignee_id },
        "labels": { "nodes": [] },
        "inverseRelations": { "nodes": [] },
        "createdAt": "2026-03-26T12:00:00Z",
        "updatedAt": "2026-03-26T13:00:00Z"
    })
}
