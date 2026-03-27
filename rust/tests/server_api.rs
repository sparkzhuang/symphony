use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use reqwest::header::CONTENT_TYPE;
use reqwest::StatusCode;
use serde_json::Value;
use symphony_rust::server::{HttpServer, ServerOptions, Snapshot, SnapshotState};
use symphony_rust::types::{
    CodexTotals, Issue, IssueId, IssueIdentifier, LiveSession, OrchestratorState, RetryEntry,
    RunAttempt, RunStatus, RunningEntry,
};
use tokio::sync::{mpsc, watch};

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

    let mut live_session = LiveSession::new("thread-1", "turn-1");
    live_session.last_codex_event = Some("turn_completed".to_owned());
    live_session.last_codex_message = Some("Working on HTTP handlers".to_owned());
    live_session.last_codex_timestamp = Some(Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap());
    live_session.codex_input_tokens = 1200;
    live_session.codex_output_tokens = 800;
    live_session.codex_total_tokens = 2000;
    live_session.turn_count = 7;

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
        live_session: Some(live_session),
        worker_host: Some("worker-a".to_owned()),
    };

    let retrying_issue_id = IssueId::new("issue-2");
    let retrying_identifier = IssueIdentifier::new("SPA-18");

    let state = OrchestratorState {
        poll_interval_ms: 30_000,
        max_concurrent_agents: 4,
        running: HashMap::from([(issue.id.clone(), running)]),
        claimed: HashSet::from([issue.id.clone(), retrying_issue_id.clone()]),
        retry_attempts: HashMap::from([(
            retrying_issue_id.clone(),
            RetryEntry {
                issue_id: retrying_issue_id,
                identifier: retrying_identifier,
                attempt: 3,
                due_at_ms: 90_000,
                timer_handle: None,
                error: Some("no available orchestrator slots".to_owned()),
            },
        )]),
        completed: HashSet::new(),
        codex_totals: CodexTotals {
            input_tokens: 5000,
            output_tokens: 2400,
            total_tokens: 7400,
            seconds_running: 1834,
        },
        codex_rate_limits: Some(serde_json::json!({
            "requests_remaining": 12,
            "tokens_remaining": 24_000
        })),
    };

    Snapshot::new(
        state,
        Utc.with_ymd_and_hms(2026, 3, 26, 10, 0, 0).unwrap(),
        10_000,
    )
}

async fn spawn_server(
    snapshot_state: SnapshotState,
    snapshot_timeout: Duration,
) -> (HttpServer, mpsc::Receiver<()>, watch::Sender<SnapshotState>) {
    let (snapshot_tx, snapshot_rx) = watch::channel(snapshot_state);
    let (refresh_tx, refresh_rx) = mpsc::channel(1);

    let server = HttpServer::bind(
        ServerOptions::new(snapshot_rx, refresh_tx)
            .with_workspace_root("/tmp/symphony_workspaces")
            .with_snapshot_timeout(snapshot_timeout),
    )
    .await
    .expect("server should bind");

    (server, refresh_rx, snapshot_tx)
}

async fn read_json(response: reqwest::Response) -> Value {
    response
        .json()
        .await
        .expect("response should be valid json")
}

#[test]
fn cli_port_overrides_config_port() {
    let options = ServerOptions::new(watch::channel(SnapshotState::Pending).1, mpsc::channel(1).0)
        .with_cli_port(Some(0))
        .with_config_port(Some(4000));

    assert_eq!(options.effective_port(), Some(0));
}

#[tokio::test]
async fn defaults_to_loopback_and_uses_ephemeral_port() {
    let (server, _refresh_rx, _snapshot_tx) = spawn_server(
        SnapshotState::Ready(Box::new(sample_state())),
        Duration::from_millis(50),
    )
    .await;

    assert!(server.local_addr().ip().is_loopback());
    assert_ne!(server.local_addr().port(), 0);

    let response = reqwest::get(format!("http://{}/healthz", server.local_addr()))
        .await
        .expect("health check should succeed");

    assert_eq!(response.status(), StatusCode::OK);

    server.shutdown().await;
}

#[tokio::test]
async fn state_endpoint_returns_running_retrying_totals_and_rate_limits() {
    let (server, _refresh_rx, _snapshot_tx) = spawn_server(
        SnapshotState::Ready(Box::new(sample_state())),
        Duration::from_millis(50),
    )
    .await;

    let payload = read_json(
        reqwest::get(format!("http://{}/api/v1/state", server.local_addr()))
            .await
            .expect("state request should succeed"),
    )
    .await;

    assert_eq!(payload["counts"]["running"], 1);
    assert_eq!(payload["counts"]["retrying"], 1);
    assert_eq!(payload["running"][0]["issue_identifier"], "SPA-17");
    assert_eq!(payload["retrying"][0]["issue_identifier"], "SPA-18");
    assert_eq!(payload["codex_totals"]["total_tokens"], 7400);
    assert_eq!(payload["rate_limits"]["requests_remaining"], 12);
    let generated_at = chrono::DateTime::parse_from_rfc3339(
        payload["generated_at"]
            .as_str()
            .expect("generated_at should be a string"),
    )
    .expect("generated_at should be RFC3339");
    let due_at = chrono::DateTime::parse_from_rfc3339(
        payload["retrying"][0]["due_at"]
            .as_str()
            .expect("due_at should be a string"),
    )
    .expect("due_at should be RFC3339");

    assert_eq!((due_at - generated_at).num_seconds(), 80);

    server.shutdown().await;
}

#[tokio::test]
async fn root_serves_embedded_dashboard_html_and_assets() {
    let (server, _refresh_rx, _snapshot_tx) = spawn_server(
        SnapshotState::Ready(Box::new(sample_state())),
        Duration::from_millis(50),
    )
    .await;

    let client = reqwest::Client::new();

    let html = client
        .get(format!("http://{}/", server.local_addr()))
        .send()
        .await
        .expect("dashboard request should succeed");
    assert_eq!(html.status(), StatusCode::OK);
    assert_eq!(
        html.headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    let html_body = html.text().await.expect("html body should be readable");
    assert!(html_body.contains("Symphony Operations Dashboard"));
    assert!(html_body.contains("/assets/dashboard.css"));
    assert!(html_body.contains("/assets/dashboard.js"));
    assert!(html_body.contains("issue-details"));

    let css = client
        .get(format!(
            "http://{}/assets/dashboard.css",
            server.local_addr()
        ))
        .send()
        .await
        .expect("css request should succeed");
    assert_eq!(css.status(), StatusCode::OK);
    assert_eq!(
        css.headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/css; charset=utf-8")
    );
    assert!(css
        .text()
        .await
        .expect("css body should be readable")
        .contains(".dashboard-shell"));

    let js = client
        .get(format!(
            "http://{}/assets/dashboard.js",
            server.local_addr()
        ))
        .send()
        .await
        .expect("js request should succeed");
    assert_eq!(js.status(), StatusCode::OK);
    assert_eq!(
        js.headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/javascript; charset=utf-8")
    );
    let js_body = js.text().await.expect("js body should be readable");
    assert!(js_body.contains("new EventSource('/api/v1/events')"));
    assert!(js_body.contains("renderIssueDetails"));

    server.shutdown().await;
}

#[tokio::test]
async fn sse_endpoint_streams_state_payload_updates() {
    let initial_snapshot = sample_state();
    let updated_snapshot = sample_state();
    let updated_snapshot = Snapshot::new(
        updated_snapshot.state.as_ref().clone(),
        updated_snapshot.observed_at,
        updated_snapshot.monotonic_now_ms + 5_000,
    );

    let (server, _refresh_rx, snapshot_tx) = spawn_server(
        SnapshotState::Ready(Box::new(initial_snapshot)),
        Duration::from_millis(50),
    )
    .await;

    let client = reqwest::Client::new();
    let mut response = client
        .get(format!("http://{}/api/v1/events", server.local_addr()))
        .send()
        .await
        .expect("sse request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    snapshot_tx
        .send(SnapshotState::Ready(Box::new(updated_snapshot)))
        .expect("snapshot update should be published");

    let first_chunk = tokio::time::timeout(Duration::from_secs(2), response.chunk())
        .await
        .expect("sse chunk should arrive")
        .expect("chunk read should succeed")
        .expect("sse stream should produce a chunk");
    let first_event = String::from_utf8(first_chunk.to_vec()).expect("chunk should be utf-8");

    assert!(first_event.contains("event: state"));
    assert!(first_event.contains("\"issue_identifier\":\"SPA-17\""));
    assert!(first_event.contains("\"retrying\":1"));
    assert!(first_event.contains("\"total_tokens\":7400"));

    drop(response);
    server.shutdown().await;
}

#[tokio::test]
async fn issue_endpoint_returns_detail_or_404() {
    let (server, _refresh_rx, _snapshot_tx) = spawn_server(
        SnapshotState::Ready(Box::new(sample_state())),
        Duration::from_millis(50),
    )
    .await;

    let existing = read_json(
        reqwest::get(format!("http://{}/api/v1/SPA-17", server.local_addr()))
            .await
            .expect("issue request should succeed"),
    )
    .await;

    assert_eq!(existing["issue_identifier"], "SPA-17");
    assert_eq!(existing["status"], "running");
    assert_eq!(existing["running"]["session_id"], "thread-1-turn-1");
    assert_eq!(existing["retry"], Value::Null);
    assert_eq!(existing["workspace"]["path"], "/tmp/symphony/SPA-17");

    let retrying = read_json(
        reqwest::get(format!("http://{}/api/v1/SPA-18", server.local_addr()))
            .await
            .expect("retrying issue request should succeed"),
    )
    .await;

    assert_eq!(retrying["issue_identifier"], "SPA-18");
    assert_eq!(retrying["status"], "retrying");
    assert_eq!(retrying["running"], Value::Null);
    assert_eq!(retrying["retry"]["attempt"], 3);
    assert_eq!(
        retrying["workspace"]["path"],
        "/tmp/symphony_workspaces/SPA-18"
    );

    let missing = reqwest::get(format!("http://{}/api/v1/SPA-404", server.local_addr()))
        .await
        .expect("missing issue request should succeed");

    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        read_json(missing).await,
        serde_json::json!({
            "error": {
                "code": "issue_not_found",
                "message": "Issue not found"
            }
        })
    );

    server.shutdown().await;
}

#[tokio::test]
async fn refresh_endpoint_returns_accepted_and_coalesces_when_queue_is_full() {
    let (server, mut refresh_rx, _snapshot_tx) = spawn_server(
        SnapshotState::Ready(Box::new(sample_state())),
        Duration::from_millis(50),
    )
    .await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("http://{}/api/v1/refresh", server.local_addr()))
        .send()
        .await
        .expect("refresh request should succeed");
    let first_payload = read_json(first).await;

    assert_eq!(first_payload["queued"], true);
    assert_eq!(first_payload["coalesced"], false);
    assert_eq!(refresh_rx.recv().await, Some(()));

    let blocking = client
        .post(format!("http://{}/api/v1/refresh", server.local_addr()))
        .send()
        .await
        .expect("second refresh request should succeed");
    let blocking_payload = read_json(blocking).await;

    assert_eq!(blocking_payload["queued"], true);
    assert_eq!(blocking_payload["coalesced"], false);

    let coalesced = client
        .post(format!("http://{}/api/v1/refresh", server.local_addr()))
        .send()
        .await
        .expect("third refresh request should succeed");
    let coalesced_payload = read_json(coalesced).await;

    assert_eq!(coalesced_payload["queued"], true);
    assert_eq!(coalesced_payload["coalesced"], true);

    server.shutdown().await;
}

#[tokio::test]
async fn unsupported_methods_return_405_with_error_envelope() {
    let (server, _refresh_rx, _snapshot_tx) = spawn_server(
        SnapshotState::Ready(Box::new(sample_state())),
        Duration::from_millis(50),
    )
    .await;
    let client = reqwest::Client::new();

    let state_response = client
        .post(format!("http://{}/api/v1/state", server.local_addr()))
        .send()
        .await
        .expect("request should succeed");
    let refresh_response = client
        .get(format!("http://{}/api/v1/refresh", server.local_addr()))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(state_response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(refresh_response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        read_json(state_response).await,
        serde_json::json!({
            "error": {
                "code": "method_not_allowed",
                "message": "Method not allowed"
            }
        })
    );
    assert_eq!(
        read_json(refresh_response).await,
        serde_json::json!({
            "error": {
                "code": "method_not_allowed",
                "message": "Method not allowed"
            }
        })
    );

    server.shutdown().await;
}

#[tokio::test]
async fn state_endpoint_surfaces_snapshot_timeout_errors() {
    let (server, _refresh_rx, _snapshot_tx) =
        spawn_server(SnapshotState::Pending, Duration::from_millis(5)).await;

    let response = reqwest::get(format!("http://{}/api/v1/state", server.local_addr()))
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        read_json(response).await["error"],
        serde_json::json!({
            "code": "snapshot_timeout",
            "message": "Snapshot timed out"
        })
    );

    server.shutdown().await;
}
