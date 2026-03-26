use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use crate::tracker::{
    AssigneeMode, LinearTrackerConfig, Tracker, TrackerError, TrackerFuture, TrackerResult,
    DEFAULT_LINEAR_PAGE_SIZE,
};
use crate::types::{BlockerRef, Issue, IssueId, IssueIdentifier};

const CANDIDATE_QUERY: &str = r#"
query SymphonyLinearPoll($projectSlug: String!, $stateNames: [String!]!, $first: Int!, $relationFirst: Int!, $after: String) {
  issues(
    filter: {project: {slugId: {eq: $projectSlug}}, state: {name: {in: $stateNames}}},
    first: $first,
    after: $after
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      state {
        name
      }
      branchName
      url
      assignee {
        id
      }
      labels {
        nodes {
          name
        }
      }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue {
            id
            identifier
            state {
              name
            }
          }
        }
      }
      createdAt
      updatedAt
    }
    pageInfo {
      hasNextPage
      endCursor
    }
  }
}
"#;

const ISSUES_BY_ID_QUERY: &str = r#"
query SymphonyLinearIssuesById($ids: [ID!]!, $first: Int!, $relationFirst: Int!) {
  issues(filter: {id: {in: $ids}}, first: $first) {
    nodes {
      id
      identifier
      title
      description
      priority
      state {
        name
      }
      branchName
      url
      assignee {
        id
      }
      labels {
        nodes {
          name
        }
      }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue {
            id
            identifier
            state {
              name
            }
          }
        }
      }
      createdAt
      updatedAt
    }
  }
}
"#;

const VIEWER_QUERY: &str = r#"
query SymphonyLinearViewer {
  viewer {
    id
  }
}
"#;

const CREATE_COMMENT_MUTATION: &str = r#"
mutation SymphonyCreateComment($issueId: String!, $body: String!) {
  commentCreate(input: {issueId: $issueId, body: $body}) {
    success
  }
}
"#;

const UPDATE_STATE_MUTATION: &str = r#"
mutation SymphonyUpdateIssueState($issueId: String!, $stateId: String!) {
  issueUpdate(id: $issueId, input: {stateId: $stateId}) {
    success
  }
}
"#;

const RESOLVE_STATE_QUERY: &str = r#"
query SymphonyResolveStateId($issueId: String!, $stateName: String!) {
  issue(id: $issueId) {
    team {
      states(filter: {name: {eq: $stateName}}, first: 1) {
        nodes {
          id
        }
      }
    }
  }
}
"#;

#[derive(Debug, Clone, PartialEq)]
pub struct GraphqlRequest {
    pub query: String,
    pub variables: Value,
    pub operation_name: Option<String>,
}

impl GraphqlRequest {
    fn new(query: &str, variables: Value) -> Self {
        Self {
            query: query.to_owned(),
            variables,
            operation_name: operation_name(query),
        }
    }

    fn payload(&self) -> Value {
        json!({
            "query": self.query,
            "variables": self.variables,
            "operationName": self.operation_name,
        })
    }
}

pub trait LinearGraphqlExecutor: Send + Sync {
    fn execute(&self, request: GraphqlRequest) -> TrackerFuture<'_, Value>;
}

#[derive(Clone)]
pub struct LinearTracker {
    config: LinearTrackerConfig,
    executor: Arc<dyn LinearGraphqlExecutor>,
}

impl std::fmt::Debug for LinearTracker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinearTracker")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl LinearTracker {
    pub fn new(config: LinearTrackerConfig) -> Self {
        let executor = ReqwestLinearGraphqlExecutor::new(
            config.endpoint.clone(),
            config.api_key.clone(),
            config.timeout,
        );

        Self {
            config,
            executor: Arc::new(executor),
        }
    }

    pub fn with_executor<E>(config: LinearTrackerConfig, executor: E) -> Self
    where
        E: LinearGraphqlExecutor + 'static,
    {
        Self {
            config,
            executor: Arc::new(executor),
        }
    }

    async fn fetch_by_states(
        &self,
        state_names: &[String],
        assignee_filter: Option<&str>,
    ) -> TrackerResult<Vec<Issue>> {
        let project_slug = self.project_slug()?;
        let mut cursor: Option<String> = None;
        let mut issues = Vec::new();

        loop {
            let request = GraphqlRequest::new(
                CANDIDATE_QUERY,
                json!({
                    "projectSlug": project_slug,
                    "stateNames": state_names,
                    "first": self.page_size(),
                    "relationFirst": self.page_size(),
                    "after": cursor,
                }),
            );
            let response = self.execute_graphql(request).await?;
            let connection = response
                .get("data")
                .and_then(|data| data.get("issues"))
                .ok_or(TrackerError::LinearUnknownPayload)?;

            let page_issues = connection
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or(TrackerError::LinearUnknownPayload)?;

            issues.extend(
                page_issues
                    .iter()
                    .filter_map(|issue| normalize_issue(issue, assignee_filter)),
            );

            let page_info = connection
                .get("pageInfo")
                .ok_or(TrackerError::LinearUnknownPayload)?;
            let has_next_page = page_info
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .ok_or(TrackerError::LinearUnknownPayload)?;

            if has_next_page {
                cursor = Some(
                    page_info
                        .get("endCursor")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or(TrackerError::LinearMissingEndCursor)?,
                );
            } else {
                return Ok(issues);
            }
        }
    }

    async fn resolve_assignee_filter(&self) -> TrackerResult<Option<String>> {
        match &self.config.assignee {
            AssigneeMode::Any => Ok(None),
            AssigneeMode::Id(id) => Ok(Some(id.clone())),
            AssigneeMode::Me => {
                let response = self
                    .execute_graphql(GraphqlRequest::new(VIEWER_QUERY, json!({})))
                    .await?;

                response
                    .get("data")
                    .and_then(|data| data.get("viewer"))
                    .and_then(|viewer| viewer.get("id"))
                    .and_then(Value::as_str)
                    .map(|id| Some(id.to_owned()))
                    .ok_or(TrackerError::LinearUnknownPayload)
            }
        }
    }

    async fn fetch_issue_states_by_id_batches(
        &self,
        issue_ids: &[String],
        assignee_filter: Option<&str>,
    ) -> TrackerResult<Vec<Issue>> {
        let mut issues_by_id = HashMap::new();

        for batch in issue_ids.chunks(DEFAULT_LINEAR_PAGE_SIZE) {
            let request = GraphqlRequest::new(
                ISSUES_BY_ID_QUERY,
                json!({
                    "ids": batch,
                    "first": batch.len(),
                    "relationFirst": self.page_size(),
                }),
            );
            let response = self.execute_graphql(request).await?;
            let nodes = response
                .get("data")
                .and_then(|data| data.get("issues"))
                .and_then(|issues| issues.get("nodes"))
                .and_then(Value::as_array)
                .ok_or(TrackerError::LinearUnknownPayload)?;

            for issue in nodes
                .iter()
                .filter_map(|issue| normalize_issue(issue, assignee_filter))
            {
                issues_by_id.insert(issue.id.as_str().to_owned(), issue);
            }
        }

        Ok(issue_ids
            .iter()
            .filter_map(|issue_id| issues_by_id.remove(issue_id))
            .collect())
    }

    async fn execute_graphql(&self, request: GraphqlRequest) -> TrackerResult<Value> {
        let response = self.executor.execute(request).await?;

        match response.get("errors").and_then(Value::as_array) {
            Some(errors) if !errors.is_empty() => {
                let messages = errors
                    .iter()
                    .map(|error| {
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown GraphQL error")
                            .to_owned()
                    })
                    .collect();

                Err(TrackerError::LinearGraphqlErrors(messages))
            }
            _ => Ok(response),
        }
    }

    fn api_key(&self) -> TrackerResult<&str> {
        self.config
            .api_key
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or(TrackerError::MissingApiKey)
    }

    fn project_slug(&self) -> TrackerResult<&str> {
        self.config
            .project_slug
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or(TrackerError::MissingProjectSlug)
    }

    fn page_size(&self) -> usize {
        self.config.page_size.max(1)
    }
}

impl Tracker for LinearTracker {
    fn fetch_candidate_issues(&self) -> TrackerFuture<'_, Vec<Issue>> {
        Box::pin(async move {
            self.api_key()?;
            let assignee_filter = self.resolve_assignee_filter().await?;
            self.fetch_by_states(&self.config.active_states, assignee_filter.as_deref())
                .await
        })
    }

    fn fetch_issues_by_states<'a>(
        &'a self,
        state_names: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async move {
            if state_names.is_empty() {
                return Ok(Vec::new());
            }

            self.api_key()?;
            self.fetch_by_states(state_names, None).await
        })
    }

    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        issue_ids: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async move {
            if issue_ids.is_empty() {
                return Ok(Vec::new());
            }

            self.api_key()?;
            let assignee_filter = self.resolve_assignee_filter().await?;
            self.fetch_issue_states_by_id_batches(issue_ids, assignee_filter.as_deref())
                .await
        })
    }

    fn create_comment<'a>(&'a self, issue_id: &'a str, body: &'a str) -> TrackerFuture<'a, ()> {
        Box::pin(async move {
            self.api_key()?;
            let response = self
                .execute_graphql(GraphqlRequest::new(
                    CREATE_COMMENT_MUTATION,
                    json!({ "issueId": issue_id, "body": body }),
                ))
                .await?;

            if response
                .get("data")
                .and_then(|data| data.get("commentCreate"))
                .and_then(|result| result.get("success"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                Ok(())
            } else {
                Err(TrackerError::CommentCreateFailed)
            }
        })
    }

    fn update_issue_state<'a>(
        &'a self,
        issue_id: &'a str,
        state_name: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async move {
            self.api_key()?;
            let state_response = self
                .execute_graphql(GraphqlRequest::new(
                    RESOLVE_STATE_QUERY,
                    json!({ "issueId": issue_id, "stateName": state_name }),
                ))
                .await?;

            let state_id = state_response
                .get("data")
                .and_then(|data| data.get("issue"))
                .and_then(|issue| issue.get("team"))
                .and_then(|team| team.get("states"))
                .and_then(|states| states.get("nodes"))
                .and_then(Value::as_array)
                .and_then(|nodes| nodes.first())
                .and_then(|state| state.get("id"))
                .and_then(Value::as_str)
                .ok_or(TrackerError::StateNotFound)?;

            let update_response = self
                .execute_graphql(GraphqlRequest::new(
                    UPDATE_STATE_MUTATION,
                    json!({ "issueId": issue_id, "stateId": state_id }),
                ))
                .await?;

            if update_response
                .get("data")
                .and_then(|data| data.get("issueUpdate"))
                .and_then(|result| result.get("success"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                Ok(())
            } else {
                Err(TrackerError::IssueUpdateFailed)
            }
        })
    }
}

#[derive(Debug, Clone)]
struct ReqwestLinearGraphqlExecutor {
    endpoint: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl ReqwestLinearGraphqlExecutor {
    fn new(endpoint: String, api_key: Option<String>, timeout: std::time::Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client should build");

        Self {
            endpoint,
            api_key,
            client,
        }
    }
}

impl LinearGraphqlExecutor for ReqwestLinearGraphqlExecutor {
    fn execute(&self, request: GraphqlRequest) -> TrackerFuture<'_, Value> {
        Box::pin(async move {
            let mut builder = self
                .client
                .post(&self.endpoint)
                .header(CONTENT_TYPE, "application/json")
                .json(&request.payload());

            if let Some(api_key) = self.api_key.as_deref() {
                builder = builder.header(AUTHORIZATION, api_key);
            }

            let response = builder
                .send()
                .await
                .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;
            let status = response.status();

            if !status.is_success() {
                return Err(TrackerError::LinearApiStatus(status.as_u16()));
            }

            response
                .json::<Value>()
                .await
                .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))
        })
    }
}

#[derive(Debug, Clone)]
pub struct MockLinearGraphqlExecutor {
    requests: Arc<Mutex<Vec<GraphqlRequest>>>,
    responses: Arc<Mutex<VecDeque<TrackerResult<Value>>>>,
}

impl MockLinearGraphqlExecutor {
    pub fn from_responses(
        requests: Arc<Mutex<Vec<GraphqlRequest>>>,
        responses: Arc<Mutex<VecDeque<TrackerResult<Value>>>>,
    ) -> Self {
        Self {
            requests,
            responses,
        }
    }
}

impl LinearGraphqlExecutor for MockLinearGraphqlExecutor {
    fn execute(&self, request: GraphqlRequest) -> TrackerFuture<'_, Value> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("mock graphql requests mutex should not be poisoned")
                .push(request);

            self.responses
                .lock()
                .expect("mock graphql responses mutex should not be poisoned")
                .pop_front()
                .unwrap_or_else(|| {
                    Err(TrackerError::LinearApiRequest(
                        "no mock response queued".to_owned(),
                    ))
                })
        })
    }
}

fn operation_name(query: &str) -> Option<String> {
    query
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("query ") || line.starts_with("mutation "))
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|token| token.split('(').next().unwrap_or(token).to_owned())
}

fn normalize_issue(issue: &Value, assignee_filter: Option<&str>) -> Option<Issue> {
    if !matches_assignee(issue.get("assignee"), assignee_filter) {
        return None;
    }

    Some(Issue {
        id: IssueId::new(issue.get("id")?.as_str()?.to_owned()),
        identifier: IssueIdentifier::new(issue.get("identifier")?.as_str()?.to_owned()),
        title: issue.get("title")?.as_str()?.to_owned(),
        description: issue
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        priority: normalize_priority(issue.get("priority")),
        state: issue.get("state")?.get("name")?.as_str()?.to_owned(),
        branch_name: issue
            .get("branchName")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: issue.get("url").and_then(Value::as_str).map(str::to_owned),
        labels: normalize_labels(issue),
        blocked_by: normalize_blockers(issue),
        created_at: parse_datetime(issue.get("createdAt")),
        updated_at: parse_datetime(issue.get("updatedAt")),
    })
}

fn matches_assignee(assignee: Option<&Value>, assignee_filter: Option<&str>) -> bool {
    match assignee_filter {
        None => true,
        Some(expected_id) => assignee
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .is_some_and(|actual_id| actual_id == expected_id),
    }
}

fn normalize_priority(priority: Option<&Value>) -> Option<u8> {
    let value = priority.and_then(Value::as_f64)?;

    if value.fract() == 0.0 && value >= 0.0 && value <= u8::MAX as f64 {
        Some(value as u8)
    } else {
        None
    }
}

fn normalize_labels(issue: &Value) -> Vec<String> {
    issue
        .get("labels")
        .and_then(|labels| labels.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| label.get("name").and_then(Value::as_str))
        .map(|label| label.to_ascii_lowercase())
        .collect()
}

fn normalize_blockers(issue: &Value) -> Vec<BlockerRef> {
    issue
        .get("inverseRelations")
        .and_then(|relations| relations.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|relation| relation.get("type").and_then(Value::as_str) == Some("blocks"))
        .map(|relation| {
            let blocked_issue = relation.get("issue");

            BlockerRef {
                id: blocked_issue
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
                    .map(|id| IssueId::new(id.to_owned())),
                identifier: blocked_issue
                    .and_then(|value| value.get("identifier"))
                    .and_then(Value::as_str)
                    .map(|identifier| IssueIdentifier::new(identifier.to_owned())),
                state: blocked_issue
                    .and_then(|value| value.get("state"))
                    .and_then(|state| state.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }
        })
        .collect()
}

fn parse_datetime(value: Option<&Value>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value?.as_str()?)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}
