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

use crate::config::WorkflowConfig;
use crate::tracker::Tracker;
use crate::types::{CodexTotals, IssueIdentifier, OrchestratorState, RetryEntry};
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
            tracker,
            workspace_manager,
            runtime: OrchestratorRuntimeConfig::from_workflow(&config),
        })
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
        let terminal_issues = self
            .tracker
            .fetch_issues_by_states(&terminal_states)
            .await
            .context("failed to fetch terminal issues for startup cleanup")?;

        for issue in terminal_issues {
            self.workspace_manager
                .remove_issue_workspaces(issue.identifier.as_str())
                .await;
        }

        Ok(())
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
