use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{TimeZone, Utc};
use reqwest::StatusCode;
use serde_json::json;
use serde_json::Value;
use symphony_rust::config::WorkflowConfig;
use symphony_rust::server::{
    bind_from_workflow, empty_snapshot, runtime_channels, Snapshot, SnapshotState,
};
use symphony_rust::types::{
    CodexTotals, Issue, IssueId, IssueIdentifier, OrchestratorState, RetryEntry, RunAttempt,
    RunStatus, RunningEntry,
};

fn sample_state() -> Snapshot {
    let issue = Issue {
        id: IssueId::new("issue-1"),
        identifier: IssueIdentifier::new("SPA-17"),
        title: "HTTP API Server".to_owned(),
        description: Some("Expose orchestrator observability over HTTP.".to_owned()),
        priority: Some(1),
        state: "In Progress".to_owned(),
        branch_name: Some("feature/SPA-17".to_owned()),
        url: Some("https://linear.app/sparkzhuang/issue/SPA-17".to_owned()),
        labels: vec!["backend".to_owned()],
        blocked_by: Vec::new(),
        created_at: Some(Utc.with_ymd_and_hms(2026, 3, 26, 8, 0, 0).unwrap()),
        updated_at: Some(Utc.with_ymd_and_hms(2026, 3, 26, 8, 30, 0).unwrap()),
    };

    let running = RunningEntry {
        issue: issue.clone(),
        run_attempt: RunAttempt {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            attempt: None,
            workspace_path: "/tmp/symphony/SPA-17".into(),
            started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 30, 0).unwrap(),
            status: RunStatus::StreamingTurn,
            error: None,
        },
        live_session: None,
        worker_host: Some("worker-a".to_owned()),
    };

    let retrying_issue_id = IssueId::new("issue-2");

    Snapshot::new(
        OrchestratorState {
            poll_interval_ms: 30_000,
            max_concurrent_agents: 4,
            running: std::collections::HashMap::from([(issue.id.clone(), running)]),
            claimed: std::collections::HashSet::from([issue.id.clone(), retrying_issue_id.clone()]),
            retry_attempts: std::collections::HashMap::from([(
                retrying_issue_id.clone(),
                RetryEntry {
                    issue_id: retrying_issue_id,
                    identifier: IssueIdentifier::new("SPA-18"),
                    attempt: 2,
                    due_at_ms: 45_000,
                    timer_handle: None,
                    error: Some("busy".to_owned()),
                },
            )]),
            completed: std::collections::HashSet::new(),
            codex_totals: CodexTotals {
                input_tokens: 21,
                output_tokens: 13,
                total_tokens: 34,
                seconds_running: 55,
            },
            codex_rate_limits: Some(serde_json::json!({ "requests_remaining": 7 })),
        },
        Utc.with_ymd_and_hms(2026, 3, 26, 10, 0, 0).unwrap(),
        5_000,
    )
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique}"))
}

fn write_workflow(path: &Path, content: &str) {
    fs::write(path, content).expect("workflow file should be written");
}

#[tokio::test]
async fn workflow_config_port_starts_server_when_cli_port_is_absent() {
    let temp_dir = unique_temp_dir("server-runtime-config");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let workflow_path = temp_dir.join("WORKFLOW.md");

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
server:
  port: 0
---
Prompt
"#,
    );

    let (_runtime, snapshot_rx, refresh_tx) = runtime_channels(1);
    let server = bind_from_workflow(&workflow_path, None, snapshot_rx, refresh_tx)
        .await
        .expect("workflow-configured server should bind")
        .expect("server should be enabled");

    let response = reqwest::get(format!("http://{}/healthz", server.local_addr()))
        .await
        .expect("health check should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(server.local_addr().ip().is_loopback());

    server.shutdown().await;
}

#[tokio::test]
async fn cli_port_override_takes_precedence_over_workflow_server_port() {
    let temp_dir = unique_temp_dir("server-runtime-cli");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let workflow_path = temp_dir.join("WORKFLOW.md");

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
server:
  port: 4010
---
Prompt
"#,
    );

    let (_runtime, snapshot_rx, refresh_tx) = runtime_channels(1);
    let server = bind_from_workflow(&workflow_path, Some(0), snapshot_rx, refresh_tx)
        .await
        .expect("cli override server should bind")
        .expect("server should be enabled");

    assert_ne!(server.local_addr().port(), 4010);

    let response = reqwest::get(format!("http://{}/healthz", server.local_addr()))
        .await
        .expect("health check should succeed");

    assert_eq!(response.status(), StatusCode::OK);

    server.shutdown().await;
}

#[tokio::test]
async fn disabled_server_returns_none_when_no_cli_or_config_port_is_set() {
    let temp_dir = unique_temp_dir("server-runtime-disabled");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let workflow_path = temp_dir.join("WORKFLOW.md");

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
---
Prompt
"#,
    );

    let (_runtime, snapshot_rx, refresh_tx) = runtime_channels(1);
    let server = tokio::time::timeout(
        Duration::from_secs(2),
        bind_from_workflow(&workflow_path, None, snapshot_rx, refresh_tx),
    )
    .await
    .expect("server bind should not hang")
    .expect("workflow should load");

    assert!(server.is_none());
}

#[tokio::test]
async fn configured_startup_uses_caller_owned_channels_for_state_and_refresh() {
    let temp_dir = unique_temp_dir("server-runtime-channels");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let workflow_path = temp_dir.join("WORKFLOW.md");

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
server:
  port: 0
---
Prompt
"#,
    );

    let (mut runtime, snapshot_rx, refresh_tx) = runtime_channels(1);
    runtime
        .snapshot_sender()
        .send(SnapshotState::Ready(Box::new(sample_state())))
        .expect("snapshot should be sent");

    let server = bind_from_workflow(&workflow_path, None, snapshot_rx, refresh_tx)
        .await
        .expect("workflow-configured server should bind")
        .expect("server should be enabled");

    let state_payload: Value = reqwest::get(format!("http://{}/api/v1/state", server.local_addr()))
        .await
        .expect("state request should succeed")
        .json()
        .await
        .expect("state response should be json");

    assert_eq!(state_payload["counts"]["running"], 1);
    assert_eq!(state_payload["rate_limits"]["requests_remaining"], 7);

    let refresh_response = reqwest::Client::new()
        .post(format!("http://{}/api/v1/refresh", server.local_addr()))
        .send()
        .await
        .expect("refresh request should succeed");

    assert_eq!(refresh_response.status(), StatusCode::ACCEPTED);
    assert_eq!(runtime.refresh_receiver().recv().await, Some(()));

    server.shutdown().await;
}

#[test]
fn empty_snapshot_projects_runtime_defaults_from_workflow_config() {
    let config = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "literal-token",
            "project_slug": "SPA"
        },
        "polling": {
            "interval_ms": 45_000
        },
        "agent": {
            "max_concurrent_agents": 6
        }
    }))
    .expect("config should parse");

    let snapshot = empty_snapshot(&config);

    assert_eq!(snapshot.state.poll_interval_ms, 45_000);
    assert_eq!(snapshot.state.max_concurrent_agents, 6);
    assert!(snapshot.state.running.is_empty());
    assert!(snapshot.state.retry_attempts.is_empty());
    assert_eq!(snapshot.state.codex_totals, CodexTotals::default());
    assert_eq!(snapshot.state.codex_rate_limits, None);
}
