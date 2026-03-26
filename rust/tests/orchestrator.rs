use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use symphony_rust::config::WorkflowConfig;
use symphony_rust::orchestrator::{
    dispatch::{dispatch_sort_key, should_dispatch_issue, DispatchDecision},
    reconciliation::{reconcile_running, reconcile_stalled_runs, ReconciliationAction},
    retry::{calculate_retry_delay_ms, schedule_retry, RetryKind, RetryScheduleRequest},
    AgentCompletion, AgentCompletionStatus, AgentExecutor, AgentTaskHandle, Orchestrator,
    OrchestratorRuntimeConfig, RuntimeSnapshot,
};
use symphony_rust::tracker::{memory::MemoryTracker, Tracker, TrackerError, TrackerFuture};
use symphony_rust::types::{
    BlockerRef, CodexTotals, Issue, IssueId, IssueIdentifier, LiveSession, OrchestratorState,
    RetryEntry, RunAttempt, RunStatus, RunningEntry, WorkflowDefinition,
};
use symphony_rust::workspace::WorkspaceManager;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique}"))
}

fn issue(
    id: &str,
    identifier: &str,
    state: &str,
    priority: Option<u8>,
    created_at: Option<chrono::DateTime<Utc>>,
) -> Issue {
    Issue {
        id: IssueId::new(id),
        identifier: IssueIdentifier::new(identifier),
        title: format!("Issue {identifier}"),
        description: None,
        priority,
        state: state.to_owned(),
        branch_name: None,
        url: None,
        labels: Vec::new(),
        blocked_by: Vec::new(),
        created_at,
        updated_at: None,
    }
}

fn running_entry(issue: Issue, started_at: chrono::DateTime<Utc>) -> RunningEntry {
    RunningEntry {
        issue: issue.clone(),
        run_attempt: RunAttempt {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            attempt: None,
            workspace_path: PathBuf::from(format!("/tmp/{}", issue.identifier.as_str())),
            started_at,
            status: RunStatus::StreamingTurn,
            error: None,
        },
        live_session: Some(LiveSession::new("thread-1", "turn-1")),
        worker_host: None,
    }
}

fn running_entry_at_path(
    issue: Issue,
    started_at: chrono::DateTime<Utc>,
    workspace_path: PathBuf,
) -> RunningEntry {
    let mut entry = running_entry(issue, started_at);
    entry.run_attempt.workspace_path = workspace_path;
    entry
}

fn state_with_running(running: HashMap<IssueId, RunningEntry>) -> OrchestratorState {
    let claimed = running.keys().cloned().collect::<HashSet<_>>();
    OrchestratorState {
        poll_interval_ms: 30_000,
        max_concurrent_agents: 3,
        running,
        claimed,
        retry_attempts: HashMap::new(),
        completed: HashSet::new(),
        codex_totals: CodexTotals::default(),
        codex_rate_limits: Some(json!({"remaining": 42})),
    }
}

#[derive(Debug, Clone)]
struct FailingTerminalTracker;

impl Tracker for FailingTerminalTracker {
    fn fetch_candidate_issues(&self) -> TrackerFuture<'_, Vec<Issue>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn fetch_issues_by_states<'a>(
        &'a self,
        _state_names: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async { Err(TrackerError::LinearApiRequest("boom".to_owned())) })
    }

    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        _issue_ids: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn create_comment<'a>(&'a self, _issue_id: &'a str, _body: &'a str) -> TrackerFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn update_issue_state<'a>(
        &'a self,
        _issue_id: &'a str,
        _state_name: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Default)]
struct FakeExecutor {
    inner: Arc<Mutex<FakeExecutorState>>,
}

#[derive(Default)]
struct FakeExecutorState {
    started: Vec<String>,
    sinks: HashMap<String, Arc<dyn Fn(AgentCompletion) + Send + Sync>>,
}

impl FakeExecutor {
    async fn started_identifiers(&self) -> Vec<String> {
        self.inner.lock().await.started.clone()
    }

    async fn complete_success(&self, issue_id: &str) {
        let sink = self.inner.lock().await.sinks.get(issue_id).cloned();
        let Some(sink) = sink else {
            panic!("missing completion sink for {issue_id}");
        };
        sink(AgentCompletion {
            issue_id: IssueId::new(issue_id),
            status: AgentCompletionStatus::Succeeded { turn_count: 1 },
        });
    }
}

impl AgentExecutor for FakeExecutor {
    fn spawn(
        &self,
        issue: Issue,
        _update_sink: Arc<dyn Fn(symphony_rust::agent::AppServerEvent) + Send + Sync>,
        completion_sink: Arc<dyn Fn(AgentCompletion) + Send + Sync>,
    ) -> anyhow::Result<AgentTaskHandle> {
        let issue_id = issue.id.as_str().to_owned();
        let issue_identifier = issue.identifier.as_str().to_owned();
        let state = Arc::clone(&self.inner);

        tokio::spawn(async move {
            let mut guard = state.lock().await;
            guard.started.push(issue_identifier);
            guard.sinks.insert(issue_id, completion_sink);
        });

        Ok(AgentTaskHandle::new(Box::new(|| {})))
    }
}

async fn wait_until<F, Fut>(description: &str, timeout: Duration, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if predicate().await {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {description}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn dispatch_sort_matches_priority_created_at_identifier_order() {
    let older = issue(
        "1",
        "SPA-100",
        "Todo",
        Some(1),
        Some(Utc::now() - ChronoDuration::days(2)),
    );
    let newer = issue(
        "2",
        "SPA-101",
        "Todo",
        Some(1),
        Some(Utc::now() - ChronoDuration::days(1)),
    );
    let lower_priority = issue(
        "3",
        "SPA-099",
        "Todo",
        Some(2),
        Some(Utc::now() - ChronoDuration::days(10)),
    );

    let mut issues = vec![lower_priority, newer, older];
    issues.sort_by_key(dispatch_sort_key);

    assert_eq!(
        issues
            .into_iter()
            .map(|issue| issue.identifier.as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["SPA-100", "SPA-101", "SPA-099"]
    );
}

#[test]
fn dispatch_rejects_todo_issue_with_non_terminal_blocker() {
    let mut blocked = issue("blocked", "SPA-102", "Todo", Some(1), Some(Utc::now()));
    blocked.blocked_by = vec![BlockerRef {
        id: Some(IssueId::new("blocker")),
        identifier: Some(IssueIdentifier::new("SPA-103")),
        state: Some("In Progress".to_owned()),
    }];

    let state = state_with_running(HashMap::new());
    let runtime = OrchestratorRuntimeConfig::default();

    let decision = should_dispatch_issue(&blocked, &state, &runtime);

    assert_eq!(decision, DispatchDecision::BlockedByNonTerminalBlocker);
}

#[test]
fn dispatch_enforces_global_and_state_concurrency_limits() {
    let in_progress_issue = issue("run-1", "SPA-104", "In Progress", Some(1), Some(Utc::now()));
    let running = HashMap::from([(
        in_progress_issue.id.clone(),
        running_entry(in_progress_issue.clone(), Utc::now()),
    )]);
    let state = state_with_running(running);
    let runtime = OrchestratorRuntimeConfig {
        max_concurrent_agents: 2,
        max_concurrent_agents_by_state: HashMap::from([("in progress".to_owned(), 1)]),
        ..OrchestratorRuntimeConfig::default()
    };

    let global_candidate = issue("todo-1", "SPA-105", "Todo", Some(1), Some(Utc::now()));
    let state_capped_candidate = issue(
        "todo-2",
        "SPA-106",
        "In Progress",
        Some(1),
        Some(Utc::now()),
    );

    assert_eq!(
        should_dispatch_issue(&state_capped_candidate, &state, &runtime),
        DispatchDecision::NoStateCapacity
    );

    let runtime = OrchestratorRuntimeConfig {
        max_concurrent_agents: 1,
        max_concurrent_agents_by_state: HashMap::new(),
        ..runtime
    };

    assert_eq!(
        should_dispatch_issue(&global_candidate, &state, &runtime),
        DispatchDecision::NoGlobalCapacity
    );
}

#[test]
fn retry_delay_uses_continuation_and_exponential_backoff_with_cap() {
    assert_eq!(
        calculate_retry_delay_ms(RetryKind::Continuation, 1, 300_000),
        1_000
    );
    assert_eq!(
        calculate_retry_delay_ms(RetryKind::Failure, 1, 300_000),
        10_000
    );
    assert_eq!(
        calculate_retry_delay_ms(RetryKind::Failure, 2, 300_000),
        20_000
    );
    assert_eq!(
        calculate_retry_delay_ms(RetryKind::Failure, 6, 300_000),
        300_000
    );
}

#[test]
fn schedule_retry_overwrites_existing_entry_and_keeps_claimed_set() {
    let issue_id = IssueId::new("retry-1");
    let mut state = state_with_running(HashMap::new());
    state.claimed.insert(issue_id.clone());
    state.retry_attempts.insert(
        issue_id.clone(),
        RetryEntry {
            issue_id: issue_id.clone(),
            identifier: IssueIdentifier::new("SPA-107"),
            attempt: 1,
            due_at_ms: 10,
            timer_handle: Some("old".to_owned()),
            error: Some("old error".to_owned()),
        },
    );

    let entry = schedule_retry(
        &mut state,
        RetryScheduleRequest {
            issue_id: issue_id.clone(),
            identifier: IssueIdentifier::new("SPA-107"),
            attempt: 2,
            kind: RetryKind::Failure,
            error: Some("boom".to_owned()),
            now_ms: 1_000,
            max_retry_backoff_ms: 300_000,
            timer_token: 7,
        },
    );

    assert_eq!(entry.attempt, 2);
    assert_eq!(entry.timer_handle.as_deref(), Some("7"));
    assert_eq!(entry.error.as_deref(), Some("boom"));
    assert!(state.claimed.contains(&issue_id));
    assert_eq!(state.retry_attempts.get(&issue_id), Some(&entry));
}

#[test]
fn reconcile_stalled_runs_returns_retry_actions() {
    let started_at = Utc::now() - ChronoDuration::seconds(10);
    let issue = issue(
        "stall-1",
        "SPA-108",
        "In Progress",
        Some(1),
        Some(Utc::now()),
    );
    let running = HashMap::from([(issue.id.clone(), running_entry(issue.clone(), started_at))]);
    let mut state = state_with_running(running);

    let actions = reconcile_stalled_runs(&mut state, Utc::now(), 1_000, 300_000);

    assert_eq!(actions.len(), 2);
    assert!(matches!(
        actions.first(),
        Some(ReconciliationAction::StopAgent {
            issue_id,
            cleanup_workspace: false
        }) if issue_id == &IssueId::new("stall-1")
    ));
    assert!(matches!(
        actions.get(1),
        Some(ReconciliationAction::ScheduleRetry { issue_id, attempt: 1, .. })
            if issue_id == &IssueId::new("stall-1")
    ));
    assert!(!state.running.contains_key(&IssueId::new("stall-1")));
}

#[tokio::test]
async fn reconcile_running_issues_stops_terminal_and_non_active_runs() {
    let terminal_issue = issue(
        "done-1",
        "SPA-109",
        "In Progress",
        Some(1),
        Some(Utc::now()),
    );
    let paused_issue = issue(
        "paused-1",
        "SPA-110",
        "In Progress",
        Some(1),
        Some(Utc::now()),
    );
    let workspace_root = unique_temp_dir("orchestrator-reconcile");
    let running = HashMap::from([
        (
            terminal_issue.id.clone(),
            running_entry_at_path(
                terminal_issue.clone(),
                Utc::now(),
                workspace_root.join("SPA-109"),
            ),
        ),
        (
            paused_issue.id.clone(),
            running_entry_at_path(
                paused_issue.clone(),
                Utc::now(),
                workspace_root.join("SPA-110"),
            ),
        ),
    ]);

    let tracker = Arc::new(MemoryTracker::new(vec![
        issue("done-1", "SPA-109", "Done", Some(1), Some(Utc::now())),
        issue("paused-1", "SPA-110", "Backlog", Some(1), Some(Utc::now())),
    ]));
    tokio::fs::create_dir_all(workspace_root.join("SPA-109"))
        .await
        .expect("terminal workspace should exist");
    tokio::fs::create_dir_all(workspace_root.join("SPA-110"))
        .await
        .expect("non-active workspace should exist");

    let workspace_manager =
        WorkspaceManager::new(&workspace_root, Default::default()).expect("workspace manager");
    let mut state = state_with_running(running);
    let runtime = OrchestratorRuntimeConfig::default();

    let actions = reconcile_running(&mut state, tracker.as_ref(), &workspace_manager, &runtime)
        .await
        .expect("reconciliation should succeed");

    assert_eq!(actions.len(), 2);
    assert!(!tokio::fs::try_exists(workspace_root.join("SPA-109"))
        .await
        .expect("terminal workspace stat should succeed"));
    assert!(tokio::fs::try_exists(workspace_root.join("SPA-110"))
        .await
        .expect("non-active workspace stat should succeed"));
}

#[tokio::test]
async fn startup_cleanup_removes_terminal_issue_workspaces() {
    let terminal = issue("cleanup-1", "SPA-111", "Done", Some(1), Some(Utc::now()));
    let active = issue(
        "cleanup-2",
        "SPA-112",
        "In Progress",
        Some(1),
        Some(Utc::now()),
    );
    let tracker = Arc::new(MemoryTracker::new(vec![terminal, active]));
    let workspace_root = unique_temp_dir("orchestrator-startup-cleanup");
    tokio::fs::create_dir_all(workspace_root.join("SPA-111"))
        .await
        .expect("terminal workspace should exist");
    tokio::fs::create_dir_all(workspace_root.join("SPA-112"))
        .await
        .expect("active workspace should exist");

    let config = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "token",
            "project_slug": "SPA",
            "terminal_states": ["Done", "Canceled"]
        },
        "workspace": {
            "root": workspace_root
        }
    }))
    .expect("config should parse");

    let orchestrator = Orchestrator::new(config, tracker).expect("orchestrator should build");
    orchestrator
        .startup_cleanup_terminal_workspaces()
        .await
        .expect("startup cleanup should succeed");

    assert!(
        !tokio::fs::try_exists(orchestrator.workspace_root().join("SPA-111"))
            .await
            .expect("terminal workspace stat should succeed")
    );
    assert!(
        tokio::fs::try_exists(orchestrator.workspace_root().join("SPA-112"))
            .await
            .expect("active workspace stat should succeed")
    );
}

#[tokio::test]
async fn startup_cleanup_ignores_tracker_failures() {
    let workspace_root = unique_temp_dir("orchestrator-startup-cleanup-failure");
    tokio::fs::create_dir_all(workspace_root.join("SPA-115"))
        .await
        .expect("workspace should exist");

    let config = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "token",
            "project_slug": "SPA",
            "terminal_states": ["Done"]
        },
        "workspace": {
            "root": workspace_root
        }
    }))
    .expect("config should parse");

    let orchestrator = Orchestrator::new(config, Arc::new(FailingTerminalTracker))
        .expect("orchestrator should build");
    orchestrator
        .startup_cleanup_terminal_workspaces()
        .await
        .expect("startup cleanup should be best effort");

    assert!(
        tokio::fs::try_exists(orchestrator.workspace_root().join("SPA-115"))
            .await
            .expect("workspace stat should succeed")
    );
}

#[test]
fn snapshot_reports_running_retrying_totals_and_rate_limits() {
    let started_at = Utc::now() - ChronoDuration::seconds(5);
    let issue = issue(
        "snap-1",
        "SPA-113",
        "In Progress",
        Some(1),
        Some(Utc::now()),
    );
    let running = HashMap::from([(issue.id.clone(), running_entry(issue.clone(), started_at))]);
    let mut state = state_with_running(running);
    let retry_issue_id = IssueId::new("snap-2");
    state.retry_attempts.insert(
        retry_issue_id.clone(),
        RetryEntry {
            issue_id: retry_issue_id,
            identifier: IssueIdentifier::new("SPA-114"),
            attempt: 2,
            due_at_ms: 25_000,
            timer_handle: Some("8".to_owned()),
            error: Some("retrying".to_owned()),
        },
    );
    state.codex_totals = CodexTotals {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        seconds_running: 7,
    };

    let snapshot = RuntimeSnapshot::from_state(&state, Utc::now(), 20_000);

    assert_eq!(snapshot.running.len(), 1);
    assert_eq!(snapshot.retrying.len(), 1);
    assert_eq!(snapshot.codex_totals.total_tokens, 15);
    assert!(snapshot.codex_totals.seconds_running >= 12);
    assert_eq!(snapshot.rate_limits, Some(json!({"remaining": 42})));
}

#[test]
fn config_reload_updates_effective_runtime_limits_immediately() {
    let initial = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "token",
            "project_slug": "SPA"
        },
        "polling": { "interval_ms": 30000 },
        "agent": {
            "max_concurrent_agents": 2,
            "max_concurrent_agents_by_state": { "Todo": 1 }
        }
    }))
    .expect("initial config should parse");
    let updated = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "token",
            "project_slug": "SPA"
        },
        "polling": { "interval_ms": 1500 },
        "agent": {
            "max_concurrent_agents": 5,
            "max_concurrent_agents_by_state": { "In Progress": 3 }
        }
    }))
    .expect("updated config should parse");

    let mut runtime = OrchestratorRuntimeConfig::from_workflow(&initial);
    runtime.apply_workflow(&updated);

    assert_eq!(runtime.poll_interval_ms, 1_500);
    assert_eq!(runtime.max_concurrent_agents, 5);
    assert_eq!(
        runtime.max_concurrent_agents_by_state,
        HashMap::from([("in progress".to_owned(), 3_usize)])
    );
}

#[tokio::test]
async fn runtime_tick_dispatches_issue_and_queues_continuation_retry() {
    let tracker = Arc::new(MemoryTracker::new(vec![issue(
        "runtime-1",
        "SPA-116",
        "Todo",
        Some(1),
        Some(Utc::now()),
    )]));
    let executor = FakeExecutor::default();
    let workflow = WorkflowDefinition::new(json!({}), "Implement the task");
    let config = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "token",
            "project_slug": "SPA"
        },
        "polling": { "interval_ms": 20 },
        "workspace": {
            "root": unique_temp_dir("orchestrator-runtime-dispatch")
        }
    }))
    .expect("config should parse");

    let handle = Orchestrator::new_with_workflow(workflow, config, tracker)
        .expect("orchestrator should build")
        .with_executor(Arc::new(executor.clone()))
        .spawn()
        .expect("runtime should spawn");

    wait_until("first issue dispatch", Duration::from_secs(2), || {
        let executor = executor.clone();
        async move { executor.started_identifiers().await.len() == 1 }
    })
    .await;

    executor.complete_success("runtime-1").await;

    wait_until("continuation retry entry", Duration::from_secs(2), || {
        let snapshot = handle.snapshot();
        async move {
            snapshot.retrying.iter().any(|entry| {
                entry.issue_id == "runtime-1" && entry.attempt == 1 && entry.remaining_ms <= 1_000
            })
        }
    })
    .await;

    handle.shutdown().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn runtime_config_reload_applies_new_capacity_immediately() {
    let tracker = Arc::new(MemoryTracker::new(vec![
        issue("reload-1", "SPA-117", "Todo", Some(1), Some(Utc::now())),
        issue("reload-2", "SPA-118", "Todo", Some(2), Some(Utc::now())),
    ]));
    let executor = FakeExecutor::default();
    let workflow = WorkflowDefinition::new(json!({}), "Implement the task");
    let initial = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "token",
            "project_slug": "SPA"
        },
        "polling": { "interval_ms": 60000 },
        "workspace": {
            "root": unique_temp_dir("orchestrator-runtime-reload")
        },
        "agent": {
            "max_concurrent_agents": 1
        }
    }))
    .expect("initial config should parse");
    let updated = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "token",
            "project_slug": "SPA"
        },
        "polling": { "interval_ms": 60000 },
        "workspace": {
            "root": unique_temp_dir("orchestrator-runtime-reload")
        },
        "agent": {
            "max_concurrent_agents": 2
        }
    }))
    .expect("updated config should parse");

    let handle = Orchestrator::new_with_workflow(workflow.clone(), initial, tracker)
        .expect("orchestrator should build")
        .with_executor(Arc::new(executor.clone()))
        .spawn()
        .expect("runtime should spawn");

    wait_until("initial dispatch", Duration::from_secs(2), || {
        let executor = executor.clone();
        async move { executor.started_identifiers().await.len() == 1 }
    })
    .await;

    handle
        .reload_config(workflow, updated)
        .map_err(|error| anyhow!("reload failed: {error}"))
        .expect("reload should enqueue");

    wait_until(
        "second dispatch after reload",
        Duration::from_secs(2),
        || {
            let executor = executor.clone();
            async move { executor.started_identifiers().await.len() == 2 }
        },
    )
    .await;

    handle.shutdown().await.expect("shutdown should succeed");
}
