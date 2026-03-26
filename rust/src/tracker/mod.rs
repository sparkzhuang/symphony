pub mod linear;
pub mod memory;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::types::Issue;

pub type TrackerResult<T> = Result<T, TrackerError>;
pub type TrackerFuture<'a, T> = Pin<Box<dyn Future<Output = TrackerResult<T>> + Send + 'a>>;

pub const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";
pub const DEFAULT_LINEAR_PAGE_SIZE: usize = 50;
pub const DEFAULT_LINEAR_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AssigneeMode {
    #[default]
    Any,
    Me,
    Id(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearTrackerConfig {
    pub endpoint: String,
    pub api_key: Option<String>,
    pub project_slug: Option<String>,
    pub active_states: Vec<String>,
    pub assignee: AssigneeMode,
    pub page_size: usize,
    pub timeout: Duration,
}

impl Default for LinearTrackerConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_LINEAR_ENDPOINT.to_owned(),
            api_key: None,
            project_slug: None,
            active_states: vec!["Todo".to_owned(), "In Progress".to_owned()],
            assignee: AssigneeMode::Any,
            page_size: DEFAULT_LINEAR_PAGE_SIZE,
            timeout: DEFAULT_LINEAR_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TrackerAdapter {
    Linear(linear::LinearTracker),
    Memory(memory::MemoryTracker),
}

pub trait Tracker: Send + Sync {
    fn fetch_candidate_issues(&self) -> TrackerFuture<'_, Vec<Issue>>;
    fn fetch_issues_by_states<'a>(
        &'a self,
        state_names: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>>;
    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        issue_ids: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>>;
    fn create_comment<'a>(&'a self, issue_id: &'a str, body: &'a str) -> TrackerFuture<'a, ()>;
    fn update_issue_state<'a>(
        &'a self,
        issue_id: &'a str,
        state_name: &'a str,
    ) -> TrackerFuture<'a, ()>;
}

impl Tracker for TrackerAdapter {
    fn fetch_candidate_issues(&self) -> TrackerFuture<'_, Vec<Issue>> {
        match self {
            Self::Linear(tracker) => tracker.fetch_candidate_issues(),
            Self::Memory(tracker) => tracker.fetch_candidate_issues(),
        }
    }

    fn fetch_issues_by_states<'a>(
        &'a self,
        state_names: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        match self {
            Self::Linear(tracker) => tracker.fetch_issues_by_states(state_names),
            Self::Memory(tracker) => tracker.fetch_issues_by_states(state_names),
        }
    }

    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        issue_ids: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        match self {
            Self::Linear(tracker) => tracker.fetch_issue_states_by_ids(issue_ids),
            Self::Memory(tracker) => tracker.fetch_issue_states_by_ids(issue_ids),
        }
    }

    fn create_comment<'a>(&'a self, issue_id: &'a str, body: &'a str) -> TrackerFuture<'a, ()> {
        match self {
            Self::Linear(tracker) => tracker.create_comment(issue_id, body),
            Self::Memory(tracker) => tracker.create_comment(issue_id, body),
        }
    }

    fn update_issue_state<'a>(
        &'a self,
        issue_id: &'a str,
        state_name: &'a str,
    ) -> TrackerFuture<'a, ()> {
        match self {
            Self::Linear(tracker) => tracker.update_issue_state(issue_id, state_name),
            Self::Memory(tracker) => tracker.update_issue_state(issue_id, state_name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerEvent {
    CommentCreated {
        issue_id: String,
        body: String,
    },
    IssueStateUpdated {
        issue_id: String,
        state_name: String,
    },
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TrackerError {
    #[error("missing_api_key")]
    MissingApiKey,
    #[error("missing_project_slug")]
    MissingProjectSlug,
    #[error("linear_api_status({0})")]
    LinearApiStatus(u16),
    #[error("linear_graphql_errors")]
    LinearGraphqlErrors(Vec<String>),
    #[error("linear_api_request: {0}")]
    LinearApiRequest(String),
    #[error("linear_unknown_payload")]
    LinearUnknownPayload,
    #[error("linear_missing_end_cursor")]
    LinearMissingEndCursor,
    #[error("comment_create_failed")]
    CommentCreateFailed,
    #[error("issue_update_failed")]
    IssueUpdateFailed,
    #[error("state_not_found")]
    StateNotFound,
}

pub type Shared<T> = Arc<T>;
