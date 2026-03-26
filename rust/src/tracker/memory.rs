use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::tracker::{Tracker, TrackerEvent, TrackerFuture};
use crate::types::Issue;

#[derive(Debug, Clone, Default)]
pub struct MemoryTracker {
    issues: Arc<Mutex<HashMap<String, Issue>>>,
    events: Arc<Mutex<Vec<TrackerEvent>>>,
}

impl MemoryTracker {
    pub fn new(issues: Vec<Issue>) -> Self {
        let issues = issues
            .into_iter()
            .map(|issue| (issue.id.as_str().to_owned(), issue))
            .collect();

        Self {
            issues: Arc::new(Mutex::new(issues)),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn recorded_events(&self) -> Vec<TrackerEvent> {
        self.events
            .lock()
            .expect("memory tracker events mutex should not be poisoned")
            .clone()
    }
}

impl Tracker for MemoryTracker {
    fn fetch_candidate_issues(&self) -> TrackerFuture<'_, Vec<Issue>> {
        Box::pin(async move {
            let issues = self
                .issues
                .lock()
                .expect("memory tracker issues mutex should not be poisoned");

            Ok(issues.values().cloned().collect())
        })
    }

    fn fetch_issues_by_states<'a>(
        &'a self,
        state_names: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async move {
            let normalized_states = normalize_state_set(state_names);
            let issues = self
                .issues
                .lock()
                .expect("memory tracker issues mutex should not be poisoned");

            Ok(issues
                .values()
                .filter(|issue| normalized_states.contains(&issue.state.to_ascii_lowercase()))
                .cloned()
                .collect())
        })
    }

    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        issue_ids: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async move {
            let issues = self
                .issues
                .lock()
                .expect("memory tracker issues mutex should not be poisoned");

            Ok(issue_ids
                .iter()
                .filter_map(|issue_id| issues.get(issue_id).cloned())
                .collect())
        })
    }

    fn create_comment<'a>(&'a self, issue_id: &'a str, body: &'a str) -> TrackerFuture<'a, ()> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("memory tracker events mutex should not be poisoned")
                .push(TrackerEvent::CommentCreated {
                    issue_id: issue_id.to_owned(),
                    body: body.to_owned(),
                });

            Ok(())
        })
    }

    fn update_issue_state<'a>(
        &'a self,
        issue_id: &'a str,
        state_name: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async move {
            {
                let mut issues = self
                    .issues
                    .lock()
                    .expect("memory tracker issues mutex should not be poisoned");

                if let Some(issue) = issues.get_mut(issue_id) {
                    issue.state = state_name.to_owned();
                }
            }

            self.events
                .lock()
                .expect("memory tracker events mutex should not be poisoned")
                .push(TrackerEvent::IssueStateUpdated {
                    issue_id: issue_id.to_owned(),
                    state_name: state_name.to_owned(),
                });

            Ok(())
        })
    }
}

fn normalize_state_set(state_names: &[String]) -> HashSet<String> {
    state_names
        .iter()
        .map(|state| state.to_ascii_lowercase())
        .collect()
}
