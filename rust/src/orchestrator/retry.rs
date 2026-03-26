use std::collections::HashMap;

use tokio::task::JoinHandle;

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

#[derive(Default)]
pub struct RetryTaskRegistry {
    tasks: HashMap<IssueId, JoinHandle<()>>,
}

impl RetryTaskRegistry {
    pub fn replace(&mut self, issue_id: IssueId, task: JoinHandle<()>) -> Option<JoinHandle<()>> {
        let previous = self.tasks.insert(issue_id, task);
        if let Some(previous) = previous {
            previous.abort();
            return Some(previous);
        }
        None
    }

    pub fn remove(&mut self, issue_id: &IssueId) -> Option<JoinHandle<()>> {
        self.tasks.remove(issue_id)
    }

    pub fn abort_all(&mut self) {
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::RetryTaskRegistry;
    use crate::types::IssueId;

    #[tokio::test]
    async fn replacing_retry_task_aborts_previous_timer() {
        let issue_id = IssueId::new("retry-task");
        let mut registry = RetryTaskRegistry::default();
        let first = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        assert!(registry.replace(issue_id.clone(), first).is_none());

        let second = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let replaced = registry
            .replace(issue_id.clone(), second)
            .expect("previous timer should be returned");

        let error = replaced
            .await
            .expect_err("replaced retry timer should be aborted");
        assert!(error.is_cancelled());
        assert!(registry.remove(&issue_id).is_some());
    }
}
