use crate::types::{IssueId, IssueIdentifier, OrchestratorState, RetryEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    Continuation,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryScheduleRequest {
    pub issue_id: IssueId,
    pub identifier: IssueIdentifier,
    pub attempt: u32,
    pub kind: RetryKind,
    pub error: Option<String>,
    pub now_ms: u64,
    pub max_retry_backoff_ms: u64,
    pub timer_token: u64,
}

pub fn calculate_retry_delay_ms(kind: RetryKind, attempt: u32, max_retry_backoff_ms: u64) -> u64 {
    match kind {
        RetryKind::Continuation => 1_000,
        RetryKind::Failure => {
            let exponent = attempt.saturating_sub(1).min(20);
            let delay = 10_000_u64.saturating_mul(1_u64 << exponent);
            delay.min(max_retry_backoff_ms)
        }
    }
}

pub fn schedule_retry(state: &mut OrchestratorState, request: RetryScheduleRequest) -> RetryEntry {
    let delay_ms =
        calculate_retry_delay_ms(request.kind, request.attempt, request.max_retry_backoff_ms);
    let entry = RetryEntry {
        issue_id: request.issue_id.clone(),
        identifier: request.identifier,
        attempt: request.attempt,
        due_at_ms: request.now_ms.saturating_add(delay_ms),
        timer_handle: Some(request.timer_token.to_string()),
        error: request.error,
    };

    state.claimed.insert(request.issue_id.clone());
    state.retry_attempts.insert(request.issue_id, entry.clone());
    entry
}
