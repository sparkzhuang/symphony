use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

use super::{RefreshQueued, Snapshot};
use crate::types::{RetryEntry, RunningEntry};

pub fn state_payload(snapshot: &Snapshot, generated_at: DateTime<Utc>) -> StatePayload {
    StatePayload {
        generated_at: iso8601(generated_at),
        counts: StateCounts {
            running: snapshot.state.running.len(),
            retrying: snapshot.state.retry_attempts.len(),
        },
        running: snapshot
            .state
            .running
            .values()
            .map(running_entry_payload)
            .collect(),
        retrying: snapshot
            .state
            .retry_attempts
            .values()
            .map(|entry| retry_entry_payload(entry, snapshot.monotonic_now_ms, generated_at))
            .collect(),
        codex_totals: CodexTotalsPayload {
            input_tokens: snapshot.state.codex_totals.input_tokens,
            output_tokens: snapshot.state.codex_totals.output_tokens,
            total_tokens: snapshot.state.codex_totals.total_tokens,
            seconds_running: snapshot.state.codex_totals.seconds_running,
        },
        rate_limits: snapshot.state.codex_rate_limits.clone(),
    }
}

pub fn state_error_payload(code: &str, message: &str, generated_at: DateTime<Utc>) -> Value {
    serde_json::json!({
        "generated_at": iso8601(generated_at),
        "error": {
            "code": code,
            "message": message
        }
    })
}

pub fn issue_payload(
    snapshot: &Snapshot,
    issue_identifier: &str,
    generated_at: DateTime<Utc>,
) -> Option<IssuePayload> {
    let running = snapshot
        .state
        .running
        .values()
        .find(|entry| entry.issue.identifier.as_str() == issue_identifier);
    let retry = snapshot
        .state
        .retry_attempts
        .values()
        .find(|entry| entry.identifier.as_str() == issue_identifier);

    if running.is_none() && retry.is_none() {
        return None;
    }

    let issue_id = running
        .map(|entry| entry.issue.id.as_str().to_owned())
        .or_else(|| retry.map(|entry| entry.issue_id.as_str().to_owned()))
        .unwrap_or_default();

    Some(IssuePayload {
        issue_identifier: issue_identifier.to_owned(),
        issue_id,
        status: issue_status(running, retry).to_owned(),
        workspace: WorkspacePayload {
            path: running
                .map(|entry| entry.run_attempt.workspace_path.display().to_string())
                .or_else(|| retry_workspace_path(issue_identifier))
                .unwrap_or_default(),
            host: running
                .and_then(|entry| entry.worker_host.clone())
                .or_else(|| retry_workspace_host(retry)),
        },
        attempts: AttemptPayload {
            restart_count: retry
                .map(|entry| entry.attempt.saturating_sub(1))
                .unwrap_or(0),
            current_retry_attempt: retry.map(|entry| entry.attempt).unwrap_or(0),
        },
        running: running.map(running_issue_payload),
        retry: retry
            .map(|entry| retry_issue_payload(entry, snapshot.monotonic_now_ms, generated_at)),
        logs: LogsPayload {
            codex_session_logs: Vec::new(),
        },
        recent_events: running.map(recent_events_payload).unwrap_or_default(),
        last_error: retry.and_then(|entry| entry.error.clone()),
        tracked: serde_json::json!({}),
    })
}

pub fn refresh_payload(result: RefreshQueued, requested_at: DateTime<Utc>) -> RefreshPayload {
    RefreshPayload {
        queued: true,
        coalesced: result.coalesced,
        requested_at: iso8601(requested_at),
        operations: vec!["poll".to_owned(), "reconcile".to_owned()],
    }
}

#[derive(Debug, Serialize)]
pub struct StatePayload {
    generated_at: String,
    counts: StateCounts,
    running: Vec<RunningEntryPayload>,
    retrying: Vec<RetryEntryPayload>,
    codex_totals: CodexTotalsPayload,
    rate_limits: Option<Value>,
}

#[derive(Debug, Serialize)]
struct StateCounts {
    running: usize,
    retrying: usize,
}

#[derive(Debug, Serialize)]
struct RunningEntryPayload {
    issue_id: String,
    issue_identifier: String,
    state: String,
    worker_host: Option<String>,
    workspace_path: String,
    session_id: Option<String>,
    turn_count: u64,
    last_event: Option<String>,
    last_message: Option<String>,
    started_at: String,
    last_event_at: Option<String>,
    tokens: TokenPayload,
}

#[derive(Debug, Serialize)]
struct RetryEntryPayload {
    issue_id: String,
    issue_identifier: String,
    attempt: u32,
    due_at: String,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodexTotalsPayload {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    seconds_running: u64,
}

#[derive(Debug, Serialize)]
pub struct IssuePayload {
    issue_identifier: String,
    issue_id: String,
    status: String,
    workspace: WorkspacePayload,
    attempts: AttemptPayload,
    running: Option<RunningIssuePayload>,
    retry: Option<RetryIssuePayload>,
    logs: LogsPayload,
    recent_events: Vec<RecentEventPayload>,
    last_error: Option<String>,
    tracked: Value,
}

#[derive(Debug, Serialize)]
struct WorkspacePayload {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
}

#[derive(Debug, Serialize)]
struct AttemptPayload {
    restart_count: u32,
    current_retry_attempt: u32,
}

#[derive(Debug, Serialize)]
struct RunningIssuePayload {
    worker_host: Option<String>,
    workspace_path: String,
    session_id: Option<String>,
    turn_count: u64,
    state: String,
    started_at: String,
    last_event: Option<String>,
    last_message: Option<String>,
    last_event_at: Option<String>,
    tokens: TokenPayload,
}

#[derive(Debug, Serialize)]
struct RetryIssuePayload {
    attempt: u32,
    due_at: String,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct LogsPayload {
    codex_session_logs: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct RecentEventPayload {
    at: String,
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenPayload {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct RefreshPayload {
    queued: bool,
    coalesced: bool,
    requested_at: String,
    operations: Vec<String>,
}

fn running_entry_payload(entry: &RunningEntry) -> RunningEntryPayload {
    let live_session = entry.live_session.as_ref();

    RunningEntryPayload {
        issue_id: entry.issue.id.as_str().to_owned(),
        issue_identifier: entry.issue.identifier.as_str().to_owned(),
        state: entry.issue.state.clone(),
        worker_host: entry.worker_host.clone(),
        workspace_path: entry.run_attempt.workspace_path.display().to_string(),
        session_id: live_session.map(|session| session.session_id.as_str().to_owned()),
        turn_count: live_session.map(|session| session.turn_count).unwrap_or(0),
        last_event: live_session.and_then(|session| session.last_codex_event.clone()),
        last_message: live_session.and_then(|session| session.last_codex_message.clone()),
        started_at: iso8601(entry.run_attempt.started_at),
        last_event_at: live_session.and_then(|session| session.last_codex_timestamp.map(iso8601)),
        tokens: token_payload(live_session),
    }
}

fn retry_entry_payload(
    entry: &RetryEntry,
    monotonic_now_ms: u64,
    generated_at: DateTime<Utc>,
) -> RetryEntryPayload {
    RetryEntryPayload {
        issue_id: entry.issue_id.as_str().to_owned(),
        issue_identifier: entry.identifier.as_str().to_owned(),
        attempt: entry.attempt,
        due_at: due_at(entry, monotonic_now_ms, generated_at),
        error: entry.error.clone(),
    }
}

fn running_issue_payload(entry: &RunningEntry) -> RunningIssuePayload {
    let live_session = entry.live_session.as_ref();

    RunningIssuePayload {
        worker_host: entry.worker_host.clone(),
        workspace_path: entry.run_attempt.workspace_path.display().to_string(),
        session_id: live_session.map(|session| session.session_id.as_str().to_owned()),
        turn_count: live_session.map(|session| session.turn_count).unwrap_or(0),
        state: entry.issue.state.clone(),
        started_at: iso8601(entry.run_attempt.started_at),
        last_event: live_session.and_then(|session| session.last_codex_event.clone()),
        last_message: live_session.and_then(|session| session.last_codex_message.clone()),
        last_event_at: live_session.and_then(|session| session.last_codex_timestamp.map(iso8601)),
        tokens: token_payload(live_session),
    }
}

fn retry_issue_payload(
    entry: &RetryEntry,
    monotonic_now_ms: u64,
    generated_at: DateTime<Utc>,
) -> RetryIssuePayload {
    RetryIssuePayload {
        attempt: entry.attempt,
        due_at: due_at(entry, monotonic_now_ms, generated_at),
        error: entry.error.clone(),
    }
}

fn recent_events_payload(entry: &RunningEntry) -> Vec<RecentEventPayload> {
    let Some(live_session) = entry.live_session.as_ref() else {
        return Vec::new();
    };

    let Some(last_event_at) = live_session.last_codex_timestamp else {
        return Vec::new();
    };

    vec![RecentEventPayload {
        at: iso8601(last_event_at),
        event: live_session
            .last_codex_event
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        message: live_session.last_codex_message.clone(),
    }]
}

fn issue_status(running: Option<&RunningEntry>, retry: Option<&RetryEntry>) -> &'static str {
    match (running, retry) {
        (Some(_), _) => "running",
        (None, Some(_)) => "retrying",
        (None, None) => "unknown",
    }
}

fn retry_workspace_path(_issue_identifier: &str) -> Option<String> {
    None
}

fn retry_workspace_host(_retry: Option<&RetryEntry>) -> Option<String> {
    None
}

fn token_payload(live_session: Option<&crate::types::LiveSession>) -> TokenPayload {
    match live_session {
        Some(session) => TokenPayload {
            input_tokens: session.codex_input_tokens,
            output_tokens: session.codex_output_tokens,
            total_tokens: session.codex_total_tokens,
        },
        None => TokenPayload {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
        },
    }
}

fn due_at(entry: &RetryEntry, monotonic_now_ms: u64, generated_at: DateTime<Utc>) -> String {
    let due_in_ms = entry.due_at_ms.saturating_sub(monotonic_now_ms);
    let due_in_ms = i64::try_from(due_in_ms).unwrap_or(i64::MAX);

    iso8601(generated_at + Duration::milliseconds(due_in_ms))
}

fn iso8601(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
