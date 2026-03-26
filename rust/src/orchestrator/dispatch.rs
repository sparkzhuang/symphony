use chrono::{DateTime, Utc};

use crate::orchestrator::{normalize_state_name, OrchestratorRuntimeConfig};
use crate::types::{Issue, OrchestratorState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchDecision {
    Eligible,
    MissingRequiredFields,
    InactiveState,
    TerminalState,
    AlreadyClaimed,
    AlreadyRunning,
    NoGlobalCapacity,
    NoStateCapacity,
    BlockedByNonTerminalBlocker,
}

pub fn dispatch_sort_key(issue: &Issue) -> (u8, i64, String) {
    (
        issue
            .priority
            .filter(|value| (1..=4).contains(value))
            .unwrap_or(5),
        created_at_sort_key(issue.created_at),
        issue.identifier.as_str().to_owned(),
    )
}

pub fn should_dispatch_issue(
    issue: &Issue,
    state: &OrchestratorState,
    runtime: &OrchestratorRuntimeConfig,
) -> DispatchDecision {
    if issue.id.as_str().trim().is_empty()
        || issue.identifier.as_str().trim().is_empty()
        || issue.title.trim().is_empty()
        || issue.state.trim().is_empty()
    {
        return DispatchDecision::MissingRequiredFields;
    }

    if runtime.is_terminal_state(&issue.state) {
        return DispatchDecision::TerminalState;
    }

    if !runtime.is_active_state(&issue.state) {
        return DispatchDecision::InactiveState;
    }

    if todo_blocked_by_non_terminal(issue, runtime) {
        return DispatchDecision::BlockedByNonTerminalBlocker;
    }

    if state.running.contains_key(&issue.id) {
        return DispatchDecision::AlreadyRunning;
    }

    if state.claimed.contains(&issue.id) {
        return DispatchDecision::AlreadyClaimed;
    }

    if state.running.len() >= runtime.max_concurrent_agents {
        return DispatchDecision::NoGlobalCapacity;
    }

    let state_limit = runtime.state_limit(&issue.state);
    let state_running_count = state
        .running
        .values()
        .filter(|entry| {
            normalize_state_name(&entry.issue.state) == normalize_state_name(&issue.state)
        })
        .count();

    if state_running_count >= state_limit {
        return DispatchDecision::NoStateCapacity;
    }

    DispatchDecision::Eligible
}

fn todo_blocked_by_non_terminal(issue: &Issue, runtime: &OrchestratorRuntimeConfig) -> bool {
    normalize_state_name(&issue.state) == "todo"
        && issue.blocked_by.iter().any(|blocker| {
            blocker
                .state
                .as_deref()
                .map(|state| !runtime.is_terminal_state(state))
                .unwrap_or(true)
        })
}

fn created_at_sort_key(created_at: Option<DateTime<Utc>>) -> i64 {
    created_at
        .map(|created_at| created_at.timestamp_micros())
        .unwrap_or(i64::MAX)
}
