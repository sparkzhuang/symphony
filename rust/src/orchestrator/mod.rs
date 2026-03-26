pub mod dispatch;
pub mod reconciliation;
pub mod retry;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};
use tracing::warn;

use crate::agent::protocol::{rate_limits_from_event, usage_from_event};
use crate::agent::{AgentRunResult, AgentRunner, AppServerEvent, AppServerEventKind};
use crate::config::WorkflowConfig;
use crate::tracker::Tracker;
use crate::types::{
    CodexTotals, Issue, IssueId, IssueIdentifier, LiveSession, OrchestratorState, RetryEntry,
    RunAttempt, RunStatus, RunningEntry, WorkflowDefinition, WorkspaceKey,
};
use crate::workspace::WorkspaceManager;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorRuntimeConfig {
    pub poll_interval_ms: u64,
    pub active_states: HashSet<String>,
    pub terminal_states: HashSet<String>,
    pub max_concurrent_agents: usize,
    pub max_concurrent_agents_by_state: HashMap<String, usize>,
    pub stall_timeout_ms: u64,
    pub max_retry_backoff_ms: u64,
}

impl OrchestratorRuntimeConfig {
    pub fn from_workflow(config: &WorkflowConfig) -> Self {
        Self {
            poll_interval_ms: config.polling.interval_ms,
            active_states: normalize_state_set(&config.tracker.active_states),
            terminal_states: normalize_state_set(&config.tracker.terminal_states),
            max_concurrent_agents: config.agent.max_concurrent_agents,
            max_concurrent_agents_by_state: config.agent.max_concurrent_agents_by_state.clone(),
            stall_timeout_ms: config.codex.stall_timeout_ms,
            max_retry_backoff_ms: config.agent.max_retry_backoff_ms,
        }
    }

    pub fn apply_workflow(&mut self, config: &WorkflowConfig) {
        *self = Self::from_workflow(config);
    }

    pub fn is_active_state(&self, state: &str) -> bool {
        self.active_states.contains(&normalize_state_name(state))
    }

    pub fn is_terminal_state(&self, state: &str) -> bool {
        self.terminal_states.contains(&normalize_state_name(state))
    }

    pub fn state_limit(&self, state: &str) -> usize {
        self.max_concurrent_agents_by_state
            .get(&normalize_state_name(state))
            .copied()
            .unwrap_or(self.max_concurrent_agents)
    }
}

impl Default for OrchestratorRuntimeConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 30_000,
            active_states: normalize_state_set(&["Todo".to_owned(), "In Progress".to_owned()]),
            terminal_states: normalize_state_set(&[
                "Closed".to_owned(),
                "Cancelled".to_owned(),
                "Canceled".to_owned(),
                "Duplicate".to_owned(),
                "Done".to_owned(),
            ]),
            max_concurrent_agents: 10,
            max_concurrent_agents_by_state: HashMap::new(),
            stall_timeout_ms: 300_000,
            max_retry_backoff_ms: 300_000,
        }
    }
}

pub struct Orchestrator {
    tracker: Arc<dyn Tracker>,
    workspace_manager: WorkspaceManager,
    runtime: OrchestratorRuntimeConfig,
    workflow: Option<WorkflowDefinition>,
    executor: Option<Arc<dyn AgentExecutor>>,
    executor_overridden: bool,
}

impl std::fmt::Debug for Orchestrator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Orchestrator")
            .field("workspace_root", &self.workspace_manager.root())
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

impl Orchestrator {
    pub fn new(config: WorkflowConfig, tracker: Arc<dyn Tracker>) -> anyhow::Result<Self> {
        let workspace_manager =
            WorkspaceManager::new(&config.workspace.root, workspace_hooks(&config))
                .context("failed to construct workspace manager")?;

        Ok(Self {
            workflow: None,
            executor: None,
            executor_overridden: false,
            tracker,
            workspace_manager,
            runtime: OrchestratorRuntimeConfig::from_workflow(&config),
        })
    }

    pub fn new_with_workflow(
        workflow: WorkflowDefinition,
        config: WorkflowConfig,
        tracker: Arc<dyn Tracker>,
    ) -> anyhow::Result<Self> {
        let workspace_manager =
            WorkspaceManager::new(&config.workspace.root, workspace_hooks(&config))
                .context("failed to construct workspace manager")?;
        let runtime = OrchestratorRuntimeConfig::from_workflow(&config);
        let executor: Arc<dyn AgentExecutor> = Arc::new(RealAgentExecutor::new(
            workflow.clone(),
            config,
            Arc::clone(&tracker),
        ));

        Ok(Self {
            tracker,
            workspace_manager,
            runtime,
            workflow: Some(workflow),
            executor: Some(executor),
            executor_overridden: false,
        })
    }

    pub fn with_executor(mut self, executor: Arc<dyn AgentExecutor>) -> Self {
        self.executor = Some(executor);
        self.executor_overridden = true;
        self
    }

    pub fn workspace_root(&self) -> &Path {
        self.workspace_manager.root()
    }

    pub fn runtime(&self) -> &OrchestratorRuntimeConfig {
        &self.runtime
    }

    pub async fn startup_cleanup_terminal_workspaces(&self) -> anyhow::Result<()> {
        let terminal_states = self
            .runtime
            .terminal_states
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let terminal_issues = match self.tracker.fetch_issues_by_states(&terminal_states).await {
            Ok(issues) => issues,
            Err(error) => {
                warn!(
                    error = %error,
                    "skipping startup terminal workspace cleanup; failed to fetch terminal issues"
                );
                return Ok(());
            }
        };

        for issue in terminal_issues {
            self.workspace_manager
                .remove_issue_workspaces(issue.identifier.as_str())
                .await;
        }

        Ok(())
    }

    pub fn spawn(self) -> anyhow::Result<OrchestratorHandle> {
        let executor = self
            .executor
            .clone()
            .context("orchestrator runtime requires an agent executor")?;
        let _workflow = self
            .workflow
            .clone()
            .context("orchestrator runtime requires workflow context")?;
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let now = Utc::now();
        let state = OrchestratorState {
            poll_interval_ms: self.runtime.poll_interval_ms,
            max_concurrent_agents: self.runtime.max_concurrent_agents,
            running: HashMap::new(),
            claimed: HashSet::new(),
            retry_attempts: HashMap::new(),
            completed: HashSet::new(),
            codex_totals: CodexTotals::default(),
            codex_rate_limits: None,
        };
        let (snapshot_tx, snapshot_rx) =
            watch::channel(RuntimeSnapshot::from_state(&state, now, 0));
        let join_handle = tokio::spawn(
            RuntimeActor {
                tracker: Arc::clone(&self.tracker),
                workspace_manager: self.workspace_manager,
                runtime: self.runtime,
                state,
                command_rx,
                command_tx: command_tx.clone(),
                snapshot_tx,
                running_tasks: HashMap::new(),
                retry_tasks: retry::RetryTaskRegistry::default(),
                next_timer_token: 0,
                tick_token: None,
                started_at: Instant::now(),
                executor,
                executor_overridden: self.executor_overridden,
            }
            .run(),
        );

        Ok(OrchestratorHandle {
            command_tx,
            snapshot_rx,
            join_handle: Some(join_handle),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCompletionStatus {
    Succeeded { turn_count: u32 },
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCompletion {
    pub issue_id: IssueId,
    pub status: AgentCompletionStatus,
}

pub type AgentUpdateSink = Arc<dyn Fn(AppServerEvent) + Send + Sync>;
pub type AgentCompletionSink = Arc<dyn Fn(AgentCompletion) + Send + Sync>;

pub trait AgentExecutor: Send + Sync {
    fn spawn(
        &self,
        issue: Issue,
        update_sink: AgentUpdateSink,
        completion_sink: AgentCompletionSink,
    ) -> anyhow::Result<AgentTaskHandle>;
}

pub struct AgentTaskHandle {
    abort: Arc<dyn Fn() + Send + Sync>,
}

impl AgentTaskHandle {
    pub fn new(abort: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            abort: Arc::from(abort),
        }
    }

    pub fn abort(&self) {
        (self.abort)();
    }
}

pub struct OrchestratorHandle {
    command_tx: mpsc::UnboundedSender<OrchestratorCommand>,
    snapshot_rx: watch::Receiver<RuntimeSnapshot>,
    join_handle: Option<JoinHandle<()>>,
}

impl OrchestratorHandle {
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot_rx.borrow().clone()
    }

    pub fn reload_config(
        &self,
        workflow: WorkflowDefinition,
        config: WorkflowConfig,
    ) -> anyhow::Result<()> {
        self.command_tx
            .send(OrchestratorCommand::ConfigReloaded {
                workflow,
                config: Box::new(config),
            })
            .context("failed to enqueue config reload")
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        let _ = self.command_tx.send(OrchestratorCommand::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            join_handle.await.context("orchestrator task join failed")?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RealAgentExecutor {
    workflow: WorkflowDefinition,
    config: WorkflowConfig,
    tracker: Arc<dyn Tracker>,
}

impl RealAgentExecutor {
    fn new(
        workflow: WorkflowDefinition,
        config: WorkflowConfig,
        tracker: Arc<dyn Tracker>,
    ) -> Self {
        Self {
            workflow,
            config,
            tracker,
        }
    }
}

impl AgentExecutor for RealAgentExecutor {
    fn spawn(
        &self,
        issue: Issue,
        update_sink: AgentUpdateSink,
        completion_sink: AgentCompletionSink,
    ) -> anyhow::Result<AgentTaskHandle> {
        let workflow = self.workflow.clone();
        let config = self.config.clone();
        let tracker = Arc::clone(&self.tracker);
        let issue_id = issue.id.clone();
        let join_handle = tokio::spawn(async move {
            let outcome = async {
                let runner = AgentRunner::new(workflow, config, tracker)
                    .context("failed to construct agent runner")?;
                runner
                    .run(issue, Some(update_sink))
                    .await
                    .map_err(anyhow::Error::from)
            }
            .await;

            let status = match outcome {
                Ok(AgentRunResult { turn_count, .. }) => {
                    AgentCompletionStatus::Succeeded { turn_count }
                }
                Err(error) => AgentCompletionStatus::Failed {
                    error: format!("{error:#}"),
                },
            };
            completion_sink(AgentCompletion { issue_id, status });
        });
        let abort_handle = join_handle.abort_handle();

        Ok(AgentTaskHandle::new(Box::new(move || abort_handle.abort())))
    }
}

#[derive(Debug)]
enum OrchestratorCommand {
    Tick {
        token: u64,
    },
    AgentCompleted(AgentCompletion),
    AgentUpdate {
        issue_id: IssueId,
        event: AppServerEvent,
    },
    RetryFire {
        issue_id: IssueId,
        token: u64,
    },
    ConfigReloaded {
        workflow: WorkflowDefinition,
        config: Box<WorkflowConfig>,
    },
    Shutdown,
}

struct RuntimeActor {
    tracker: Arc<dyn Tracker>,
    workspace_manager: WorkspaceManager,
    runtime: OrchestratorRuntimeConfig,
    state: OrchestratorState,
    command_rx: mpsc::UnboundedReceiver<OrchestratorCommand>,
    command_tx: mpsc::UnboundedSender<OrchestratorCommand>,
    snapshot_tx: watch::Sender<RuntimeSnapshot>,
    running_tasks: HashMap<IssueId, AgentTaskHandle>,
    retry_tasks: retry::RetryTaskRegistry,
    next_timer_token: u64,
    tick_token: Option<u64>,
    started_at: Instant,
    executor: Arc<dyn AgentExecutor>,
    executor_overridden: bool,
}

impl RuntimeActor {
    async fn run(mut self) {
        let _ = self.startup_cleanup().await;
        self.schedule_tick(0);
        self.publish_snapshot();

        loop {
            tokio::select! {
                maybe_command = self.command_rx.recv() => {
                    let Some(command) = maybe_command else {
                        break;
                    };

                    match command {
                        OrchestratorCommand::Tick { token } => self.handle_tick(token).await,
                        OrchestratorCommand::AgentCompleted(completion) => {
                            self.handle_agent_completed(completion);
                        }
                        OrchestratorCommand::AgentUpdate { issue_id, event } => {
                            self.handle_agent_update(&issue_id, event);
                        }
                        OrchestratorCommand::RetryFire { issue_id, token } => {
                            self.handle_retry_fire(&issue_id, token).await;
                        }
                        OrchestratorCommand::ConfigReloaded { workflow, config } => {
                            self.handle_config_reload(workflow, config);
                        }
                        OrchestratorCommand::Shutdown => {
                            self.shutdown_running_tasks();
                            self.retry_tasks.abort_all();
                            self.publish_snapshot();
                            break;
                        }
                    }

                    self.publish_snapshot();
                }
            }
        }
    }

    async fn startup_cleanup(&self) -> anyhow::Result<()> {
        let terminal_states = self
            .runtime
            .terminal_states
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let issues = match self.tracker.fetch_issues_by_states(&terminal_states).await {
            Ok(issues) => issues,
            Err(error) => {
                warn!(
                    error = %error,
                    "skipping startup terminal workspace cleanup; failed to fetch terminal issues"
                );
                return Ok(());
            }
        };

        for issue in issues {
            self.workspace_manager
                .remove_issue_workspaces(issue.identifier.as_str())
                .await;
        }

        Ok(())
    }

    async fn handle_tick(&mut self, token: u64) {
        if self.tick_token != Some(token) {
            return;
        }
        self.tick_token = None;

        self.run_reconciliation().await;
        self.dispatch_candidates().await;
        self.schedule_tick(self.runtime.poll_interval_ms);
    }

    async fn run_reconciliation(&mut self) {
        let before = self.state.running.clone();
        let stall_actions = reconciliation::reconcile_stalled_runs(
            &mut self.state,
            Utc::now(),
            self.runtime.stall_timeout_ms,
            self.runtime.max_retry_backoff_ms,
        );
        self.apply_reconciliation_actions(stall_actions, &before);

        let before = self.state.running.clone();
        match reconciliation::reconcile_running(
            &mut self.state,
            self.tracker.as_ref(),
            &self.workspace_manager,
            &self.runtime,
        )
        .await
        {
            Ok(actions) => self.apply_reconciliation_actions(actions, &before),
            Err(error) => warn!(error = %error, "running issue reconciliation failed"),
        }
    }

    fn apply_reconciliation_actions(
        &mut self,
        actions: Vec<reconciliation::ReconciliationAction>,
        previous_running: &HashMap<IssueId, RunningEntry>,
    ) {
        let mut completed = HashSet::new();
        for action in actions {
            match action {
                reconciliation::ReconciliationAction::StopAgent {
                    issue_id,
                    cleanup_workspace: _,
                } => {
                    if completed.insert(issue_id.clone()) {
                        if let Some(entry) = previous_running.get(&issue_id) {
                            self.record_session_completion_totals(entry);
                        }
                    }
                    self.stop_running_task(&issue_id);
                }
                reconciliation::ReconciliationAction::ScheduleRetry {
                    issue_id,
                    identifier,
                    attempt,
                    error,
                } => {
                    self.schedule_retry_entry(
                        issue_id,
                        identifier,
                        attempt,
                        retry::RetryKind::Failure,
                        Some(error),
                    );
                }
                reconciliation::ReconciliationAction::RefreshIssue { .. } => {}
            }
        }
    }

    async fn dispatch_candidates(&mut self) {
        let mut candidates = match self.tracker.fetch_candidate_issues().await {
            Ok(issues) => issues,
            Err(error) => {
                warn!(error = %error, "candidate issue fetch failed");
                return;
            }
        };
        candidates.sort_by_key(dispatch::dispatch_sort_key);

        for issue in candidates {
            if dispatch::should_dispatch_issue(&issue, &self.state, &self.runtime)
                != dispatch::DispatchDecision::Eligible
            {
                continue;
            }

            if let Err(error) = self.dispatch_issue(issue, None) {
                warn!(error = %error, "failed to dispatch issue");
            }
        }
    }

    fn dispatch_issue(&mut self, issue: Issue, attempt: Option<u32>) -> anyhow::Result<()> {
        let issue_id = issue.id.clone();
        let issue_identifier = issue.identifier.clone();
        let update_tx = self.command_tx.clone();
        let completion_tx = self.command_tx.clone();
        let update_issue_id = issue_id.clone();
        let completion_issue_id = issue_id.clone();
        let update_sink: AgentUpdateSink = Arc::new(move |event| {
            let _ = update_tx.send(OrchestratorCommand::AgentUpdate {
                issue_id: update_issue_id.clone(),
                event,
            });
        });
        let completion_sink: AgentCompletionSink = Arc::new(move |completion| {
            let completion = AgentCompletion {
                issue_id: completion_issue_id.clone(),
                status: completion.status,
            };
            let _ = completion_tx.send(OrchestratorCommand::AgentCompleted(completion));
        });
        let task = self
            .executor
            .spawn(issue.clone(), update_sink, completion_sink)
            .with_context(|| format!("failed to start agent for {}", issue.identifier.as_str()))?;
        let workspace_path = self
            .workspace_manager
            .root()
            .join(WorkspaceKey::from_issue_identifier(issue.identifier.as_str()).as_str());

        self.state.claimed.insert(issue_id.clone());
        self.state.retry_attempts.remove(&issue_id);
        self.state.running.insert(
            issue_id.clone(),
            RunningEntry {
                issue,
                run_attempt: RunAttempt {
                    issue_id: issue_id.clone(),
                    issue_identifier,
                    attempt,
                    workspace_path,
                    started_at: Utc::now(),
                    status: RunStatus::LaunchingAgentProcess,
                    error: None,
                },
                live_session: None,
                worker_host: None,
            },
        );
        self.running_tasks.insert(issue_id, task);
        Ok(())
    }

    fn handle_agent_update(&mut self, issue_id: &IssueId, event: AppServerEvent) {
        let Some(entry) = self.state.running.get_mut(issue_id) else {
            return;
        };

        if matches!(event.kind, AppServerEventKind::SessionStarted) {
            let thread_id = event
                .payload
                .get("thread_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let turn_id = event
                .payload
                .get("turn_id")
                .and_then(Value::as_str)
                .unwrap_or_default();

            if !thread_id.is_empty() && !turn_id.is_empty() {
                let live_session = entry
                    .live_session
                    .get_or_insert_with(|| LiveSession::new(thread_id, turn_id));
                live_session.thread_id = thread_id.to_owned();
                live_session.turn_id = turn_id.to_owned();
                live_session.session_id = crate::types::SessionId::from_parts(thread_id, turn_id);
                live_session.codex_app_server_pid = event
                    .payload
                    .get("codex_app_server_pid")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }

        let live_session = entry
            .live_session
            .get_or_insert_with(|| LiveSession::new("unknown-thread", "unknown-turn"));
        live_session.last_codex_timestamp = Some(event.timestamp);
        live_session.last_codex_event = Some(event_kind_name(&event.kind));
        live_session.last_codex_message = event_message_summary(&event.payload);

        if let Some(usage) = usage_from_event(&event.payload) {
            apply_usage_update(live_session, &usage);
        }

        if let Some(rate_limits) = rate_limits_from_event(&event.payload) {
            self.state.codex_rate_limits = Some(rate_limits);
        }
    }

    fn handle_agent_completed(&mut self, completion: AgentCompletion) {
        let Some(entry) = self.state.running.remove(&completion.issue_id) else {
            return;
        };
        self.running_tasks.remove(&completion.issue_id);
        self.record_session_completion_totals(&entry);
        self.state.claimed.remove(&completion.issue_id);

        match completion.status {
            AgentCompletionStatus::Succeeded { .. } => {
                self.state.completed.insert(completion.issue_id.clone());
                self.schedule_retry_entry(
                    completion.issue_id,
                    entry.issue.identifier,
                    1,
                    retry::RetryKind::Continuation,
                    None,
                );
            }
            AgentCompletionStatus::Failed { error } => {
                self.schedule_retry_entry(
                    completion.issue_id,
                    entry.issue.identifier,
                    next_retry_attempt(entry.run_attempt.attempt),
                    retry::RetryKind::Failure,
                    Some(error),
                );
            }
        }
    }

    async fn handle_retry_fire(&mut self, issue_id: &IssueId, token: u64) {
        let token_string = token.to_string();
        let Some(retry_entry) = self.state.retry_attempts.get(issue_id).cloned() else {
            return;
        };
        if retry_entry.timer_handle.as_deref() != Some(token_string.as_str()) {
            return;
        }

        let _ = self.retry_tasks.remove(issue_id);
        self.state.retry_attempts.remove(issue_id);
        self.state.claimed.remove(issue_id);

        let issues = match self.tracker.fetch_candidate_issues().await {
            Ok(issues) => issues,
            Err(error) => {
                self.schedule_retry_entry(
                    retry_entry.issue_id,
                    retry_entry.identifier,
                    retry_entry.attempt.saturating_add(1),
                    retry::RetryKind::Failure,
                    Some(format!("retry poll failed: {error}")),
                );
                return;
            }
        };

        let Some(issue) = issues.into_iter().find(|issue| issue.id == *issue_id) else {
            return;
        };

        if self.runtime.is_terminal_state(&issue.state) {
            self.workspace_manager
                .remove_issue_workspaces(issue.identifier.as_str())
                .await;
            return;
        }

        if !self.runtime.is_active_state(&issue.state) {
            return;
        }

        match dispatch::should_dispatch_issue(&issue, &self.state, &self.runtime) {
            dispatch::DispatchDecision::Eligible => {
                if let Err(error) = self.dispatch_issue(issue, Some(retry_entry.attempt)) {
                    self.schedule_retry_entry(
                        retry_entry.issue_id,
                        retry_entry.identifier,
                        retry_entry.attempt.saturating_add(1),
                        retry::RetryKind::Failure,
                        Some(format!("retry dispatch failed: {error:#}")),
                    );
                }
            }
            decision => {
                self.schedule_retry_entry(
                    retry_entry.issue_id,
                    retry_entry.identifier,
                    retry_entry.attempt.saturating_add(1),
                    retry::RetryKind::Failure,
                    Some(format!(
                        "retry deferred: {}",
                        dispatch_decision_reason(decision)
                    )),
                );
            }
        }
    }

    fn handle_config_reload(&mut self, workflow: WorkflowDefinition, config: Box<WorkflowConfig>) {
        self.runtime.apply_workflow(&config);
        self.state.poll_interval_ms = self.runtime.poll_interval_ms;
        self.state.max_concurrent_agents = self.runtime.max_concurrent_agents;
        if !self.executor_overridden {
            self.executor = Arc::new(RealAgentExecutor::new(
                workflow,
                *config,
                Arc::clone(&self.tracker),
            ));
        }
        self.schedule_tick(0);
    }

    fn schedule_tick(&mut self, delay_ms: u64) {
        let token = self.next_token();
        self.tick_token = Some(token);
        let command_tx = self.command_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let _ = command_tx.send(OrchestratorCommand::Tick { token });
        });
    }

    fn schedule_retry_entry(
        &mut self,
        issue_id: IssueId,
        identifier: IssueIdentifier,
        attempt: u32,
        kind: retry::RetryKind,
        error: Option<String>,
    ) {
        let timer_token = self.next_token();
        let now_ms = self.now_millis();
        let entry = retry::schedule_retry(
            &mut self.state,
            retry::RetryScheduleRequest {
                issue_id: issue_id.clone(),
                identifier,
                attempt,
                kind,
                error,
                now_ms,
                max_retry_backoff_ms: self.runtime.max_retry_backoff_ms,
                timer_token,
            },
        );
        let command_tx = self.command_tx.clone();
        let delay_ms = entry.due_at_ms.saturating_sub(self.now_millis());
        let retry_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let _ = command_tx.send(OrchestratorCommand::RetryFire {
                issue_id,
                token: timer_token,
            });
        });
        let _ = self.retry_tasks.replace(entry.issue_id.clone(), retry_task);
    }

    fn stop_running_task(&mut self, issue_id: &IssueId) {
        if let Some(task) = self.running_tasks.remove(issue_id) {
            task.abort();
        }
    }

    fn shutdown_running_tasks(&mut self) {
        for (_, task) in self.running_tasks.drain() {
            task.abort();
        }
    }

    fn record_session_completion_totals(&mut self, entry: &RunningEntry) {
        if let Some(live_session) = entry.live_session.as_ref() {
            self.state.codex_totals.input_tokens = self
                .state
                .codex_totals
                .input_tokens
                .saturating_add(live_session.codex_input_tokens);
            self.state.codex_totals.output_tokens = self
                .state
                .codex_totals
                .output_tokens
                .saturating_add(live_session.codex_output_tokens);
            self.state.codex_totals.total_tokens = self
                .state
                .codex_totals
                .total_tokens
                .saturating_add(live_session.codex_total_tokens);
        }

        let elapsed = Utc::now()
            .signed_duration_since(entry.run_attempt.started_at)
            .num_seconds()
            .max(0) as u64;
        self.state.codex_totals.seconds_running = self
            .state
            .codex_totals
            .seconds_running
            .saturating_add(elapsed);
    }

    fn publish_snapshot(&self) {
        let _ = self.snapshot_tx.send(RuntimeSnapshot::from_state(
            &self.state,
            Utc::now(),
            self.now_millis(),
        ));
    }

    fn next_token(&mut self) -> u64 {
        self.next_timer_token = self.next_timer_token.saturating_add(1);
        self.next_timer_token
    }

    fn now_millis(&self) -> u64 {
        self.started_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub generated_at: DateTime<Utc>,
    pub running: Vec<RunningSnapshot>,
    pub retrying: Vec<RetrySnapshot>,
    pub codex_totals: CodexTotals,
    pub rate_limits: Option<Value>,
}

impl RuntimeSnapshot {
    pub fn from_state(state: &OrchestratorState, now: DateTime<Utc>, now_millis: u64) -> Self {
        let running = state
            .running
            .values()
            .map(|entry| RunningSnapshot::from_running_entry(entry, now))
            .collect::<Vec<_>>();

        let retrying = state
            .retry_attempts
            .values()
            .map(|entry| RetrySnapshot::from_retry_entry(entry, now_millis))
            .collect::<Vec<_>>();

        let mut codex_totals = state.codex_totals.clone();
        let active_seconds = state
            .running
            .values()
            .map(|entry| {
                now.signed_duration_since(entry.run_attempt.started_at)
                    .num_seconds()
                    .max(0) as u64
            })
            .sum::<u64>();
        codex_totals.seconds_running = codex_totals.seconds_running.saturating_add(active_seconds);

        Self {
            generated_at: now,
            running,
            retrying,
            codex_totals,
            rate_limits: state.codex_rate_limits.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningSnapshot {
    pub issue_id: String,
    pub issue_identifier: String,
    pub state: String,
    pub session_id: Option<String>,
    pub turn_count: u64,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl RunningSnapshot {
    fn from_running_entry(entry: &crate::types::RunningEntry, _now: DateTime<Utc>) -> Self {
        let live_session = entry.live_session.as_ref();

        Self {
            issue_id: entry.issue.id.as_str().to_owned(),
            issue_identifier: entry.issue.identifier.as_str().to_owned(),
            state: entry.issue.state.clone(),
            session_id: live_session.map(|session| session.session_id.as_str().to_owned()),
            turn_count: live_session
                .map(|session| session.turn_count)
                .unwrap_or_default(),
            last_event: live_session.and_then(|session| session.last_codex_event.clone()),
            last_message: live_session.and_then(|session| session.last_codex_message.clone()),
            started_at: entry.run_attempt.started_at,
            last_event_at: live_session.and_then(|session| session.last_codex_timestamp),
            input_tokens: live_session
                .map(|session| session.codex_input_tokens)
                .unwrap_or_default(),
            output_tokens: live_session
                .map(|session| session.codex_output_tokens)
                .unwrap_or_default(),
            total_tokens: live_session
                .map(|session| session.codex_total_tokens)
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrySnapshot {
    pub issue_id: String,
    pub issue_identifier: String,
    pub attempt: u32,
    pub due_at_ms: u64,
    pub error: Option<String>,
    pub remaining_ms: u64,
}

impl RetrySnapshot {
    fn from_retry_entry(entry: &RetryEntry, now_millis: u64) -> Self {
        Self {
            issue_id: entry.issue_id.as_str().to_owned(),
            issue_identifier: entry.identifier.as_str().to_owned(),
            attempt: entry.attempt,
            due_at_ms: entry.due_at_ms,
            error: entry.error.clone(),
            remaining_ms: entry.due_at_ms.saturating_sub(now_millis),
        }
    }
}

fn normalize_state_set(states: &[String]) -> HashSet<String> {
    states
        .iter()
        .map(|state| normalize_state_name(state))
        .filter(|state| !state.is_empty())
        .collect()
}

pub(crate) fn normalize_state_name(state: &str) -> String {
    state.trim().to_ascii_lowercase()
}

fn workspace_hooks(config: &WorkflowConfig) -> crate::types::WorkspaceHooks {
    crate::types::WorkspaceHooks {
        after_create: config.hooks.after_create.clone(),
        before_run: config.hooks.before_run.clone(),
        after_run: config.hooks.after_run.clone(),
        before_remove: config.hooks.before_remove.clone(),
        timeout_ms: Some(config.hooks.timeout_ms),
    }
}

pub(crate) fn next_retry_attempt(attempt: Option<u32>) -> u32 {
    attempt.unwrap_or(0).saturating_add(1)
}

pub(crate) fn retry_identifier(identifier: &IssueIdentifier) -> IssueIdentifier {
    identifier.clone()
}

fn event_kind_name(kind: &AppServerEventKind) -> String {
    match kind {
        AppServerEventKind::SessionStarted => "session_started",
        AppServerEventKind::StartupFailed => "startup_failed",
        AppServerEventKind::ApprovalResolved => "approval_resolved",
        AppServerEventKind::ToolInputResolved => "tool_input_resolved",
        AppServerEventKind::UnsupportedToolCall => "unsupported_tool_call",
        AppServerEventKind::ToolCallFailed => "tool_call_failed",
        AppServerEventKind::TurnCompleted => "turn_completed",
        AppServerEventKind::TurnFailed => "turn_failed",
        AppServerEventKind::TurnCancelled => "turn_cancelled",
        AppServerEventKind::Malformed => "malformed",
        AppServerEventKind::Notification => "notification",
        AppServerEventKind::OtherMessage => "other_message",
    }
    .to_owned()
}

fn event_message_summary(payload: &Value) -> Option<String> {
    let text = serde_json::to_string(payload).ok()?;
    let mut chars = text.chars();
    let summary = chars.by_ref().take(200).collect::<String>();
    if chars.next().is_some() {
        Some(format!("{summary}..."))
    } else {
        Some(summary)
    }
}

fn apply_usage_update(live_session: &mut LiveSession, usage: &Value) {
    let input = extract_usage_value(usage, &["input_tokens", "inputTokens"]).unwrap_or(0);
    let output = extract_usage_value(usage, &["output_tokens", "outputTokens"]).unwrap_or(0);
    let total = extract_usage_value(usage, &["total_tokens", "totalTokens"])
        .unwrap_or_else(|| input.saturating_add(output));

    live_session.codex_input_tokens = input;
    live_session.codex_output_tokens = output;
    live_session.codex_total_tokens = total;
    live_session.last_reported_input_tokens = input;
    live_session.last_reported_output_tokens = output;
    live_session.last_reported_total_tokens = total;
}

fn extract_usage_value(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(Value::as_u64)
        .or_else(|| {
            value.as_object().and_then(|object| {
                object
                    .values()
                    .find_map(|nested| extract_usage_value(nested, keys))
            })
        })
}

fn dispatch_decision_reason(decision: dispatch::DispatchDecision) -> &'static str {
    match decision {
        dispatch::DispatchDecision::Eligible => "eligible",
        dispatch::DispatchDecision::MissingRequiredFields => "missing required fields",
        dispatch::DispatchDecision::InactiveState => "inactive state",
        dispatch::DispatchDecision::TerminalState => "terminal state",
        dispatch::DispatchDecision::AlreadyClaimed => "already claimed",
        dispatch::DispatchDecision::AlreadyRunning => "already running",
        dispatch::DispatchDecision::NoGlobalCapacity => "no available orchestrator slots",
        dispatch::DispatchDecision::NoStateCapacity => "state capacity exhausted",
        dispatch::DispatchDecision::BlockedByNonTerminalBlocker => {
            "blocked by non-terminal blocker"
        }
    }
}
