use std::collections::{HashMap, HashSet};

use anyhow::Context;
use chrono::{DateTime, Utc};

use crate::orchestrator::{next_retry_attempt, retry_identifier, OrchestratorRuntimeConfig};
use crate::tracker::Tracker;
use crate::types::{Issue, IssueId, IssueIdentifier, OrchestratorState, RunningEntry};
use crate::workspace::WorkspaceManager;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationAction {
    StopAgent {
        issue_id: IssueId,
        cleanup_workspace: bool,
    },
    ScheduleRetry {
        issue_id: IssueId,
        identifier: IssueIdentifier,
        attempt: u32,
        error: String,
    },
    RefreshIssue {
        issue_id: IssueId,
        state: String,
    },
}

pub fn reconcile_stalled_runs(
    state: &mut OrchestratorState,
    now: DateTime<Utc>,
    stall_timeout_ms: u64,
    _max_retry_backoff_ms: u64,
) -> Vec<ReconciliationAction> {
    if stall_timeout_ms == 0 || state.running.is_empty() {
        return Vec::new();
    }

    let stalled_issue_ids = state
        .running
        .iter()
        .filter_map(|(issue_id, entry)| {
            let last_activity = entry
                .live_session
                .as_ref()
                .and_then(|session| session.last_codex_timestamp)
                .unwrap_or(entry.run_attempt.started_at);
            let elapsed_ms = now
                .signed_duration_since(last_activity)
                .num_milliseconds()
                .max(0) as u64;

            (elapsed_ms > stall_timeout_ms).then(|| (issue_id.clone(), elapsed_ms))
        })
        .collect::<Vec<_>>();

    let mut actions = Vec::with_capacity(stalled_issue_ids.len());
    for (issue_id, elapsed_ms) in stalled_issue_ids {
        if let Some(entry) = remove_running_issue(state, &issue_id) {
            actions.push(ReconciliationAction::StopAgent {
                issue_id: issue_id.clone(),
                cleanup_workspace: false,
            });
            actions.push(ReconciliationAction::ScheduleRetry {
                issue_id,
                identifier: retry_identifier(&entry.issue.identifier),
                attempt: next_retry_attempt(entry.run_attempt.attempt),
                error: format!("stalled for {elapsed_ms}ms without codex activity"),
            });
        }
    }

    actions
}

pub async fn reconcile_running(
    state: &mut OrchestratorState,
    tracker: &dyn Tracker,
    workspace_manager: &WorkspaceManager,
    runtime: &OrchestratorRuntimeConfig,
) -> anyhow::Result<Vec<ReconciliationAction>> {
    let running_issue_ids = state
        .running
        .keys()
        .map(|issue_id| issue_id.as_str().to_owned())
        .collect::<Vec<_>>();
    if running_issue_ids.is_empty() {
        return Ok(Vec::new());
    }

    let refreshed = tracker
        .fetch_issue_states_by_ids(&running_issue_ids)
        .await
        .context("failed to refresh running issue states")?;
    let visible_ids = refreshed
        .iter()
        .map(|issue| issue.id.clone())
        .collect::<HashSet<_>>();
    let mut actions = Vec::new();

    for issue in refreshed {
        reconcile_issue_state(state, workspace_manager, runtime, issue, &mut actions).await?;
    }

    let missing_ids = state
        .running
        .keys()
        .filter(|issue_id| !visible_ids.contains(*issue_id))
        .cloned()
        .collect::<Vec<_>>();
    for issue_id in missing_ids {
        if remove_running_issue(state, &issue_id).is_some() {
            actions.push(ReconciliationAction::StopAgent {
                issue_id,
                cleanup_workspace: false,
            });
        }
    }

    Ok(actions)
}

async fn reconcile_issue_state(
    state: &mut OrchestratorState,
    workspace_manager: &WorkspaceManager,
    runtime: &OrchestratorRuntimeConfig,
    issue: Issue,
    actions: &mut Vec<ReconciliationAction>,
) -> anyhow::Result<()> {
    if runtime.is_terminal_state(&issue.state) {
        if let Some(entry) = remove_running_issue(state, &issue.id) {
            workspace_manager
                .remove(
                    &entry.run_attempt.workspace_path,
                    entry.worker_host.as_deref(),
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to remove workspace for terminal issue {}",
                        issue.identifier.as_str()
                    )
                })?;
            actions.push(ReconciliationAction::StopAgent {
                issue_id: issue.id.clone(),
                cleanup_workspace: true,
            });
        }
        return Ok(());
    }

    if runtime.is_active_state(&issue.state) {
        if let Some(entry) = state.running.get_mut(&issue.id) {
            entry.issue = issue.clone();
            actions.push(ReconciliationAction::RefreshIssue {
                issue_id: issue.id,
                state: issue.state,
            });
        }
        return Ok(());
    }

    if remove_running_issue(state, &issue.id).is_some() {
        actions.push(ReconciliationAction::StopAgent {
            issue_id: issue.id,
            cleanup_workspace: false,
        });
    }

    Ok(())
}

fn remove_running_issue(state: &mut OrchestratorState, issue_id: &IssueId) -> Option<RunningEntry> {
    state.claimed.remove(issue_id);
    state.retry_attempts.remove(issue_id);
    state.running.remove(issue_id)
}

#[allow(dead_code)]
fn _running_issue_states(state: &OrchestratorState) -> HashMap<IssueId, String> {
    state
        .running
        .iter()
        .map(|(issue_id, entry)| (issue_id.clone(), entry.issue.state.clone()))
        .collect()
}
