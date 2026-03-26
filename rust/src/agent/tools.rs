use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::config::TrackerConfig;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = DynamicToolResult> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DynamicToolSpec {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DynamicToolResult {
    pub success: bool,
    pub output: String,
    #[serde(rename = "contentItems")]
    pub content_items: Vec<Value>,
}

impl DynamicToolResult {
    pub fn success(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            success: true,
            content_items: vec![json!({"type": "inputText", "text": output.clone()})],
            output,
        }
    }

    pub fn failure(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            success: false,
            content_items: vec![json!({"type": "inputText", "text": output.clone()})],
            output,
        }
    }

    pub fn unsupported(tool: Option<&str>, specs: &[DynamicToolSpec]) -> Self {
        let supported = specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Self::failure(format!(
            "{{\"error\":{{\"message\":\"Unsupported dynamic tool: {}.\",\"supportedTools\":[{}]}}}}",
            serde_json::to_string(&tool.unwrap_or("unknown")).unwrap_or_else(|_| "\"unknown\"".to_owned()),
            specs
                .iter()
                .map(|spec| serde_json::to_string(&spec.name).unwrap_or_else(|_| "\"linear_graphql\"".to_owned()))
                .collect::<Vec<_>>()
                .join(",")
        ))
        .with_fallback_message(supported)
    }

    fn with_fallback_message(mut self, supported: String) -> Self {
        if !self.output.contains("supportedTools") {
            self.output = format!(
                "{{\"error\":{{\"message\":\"Unsupported dynamic tool.\",\"supportedTools\":\"{}\"}}}}",
                supported
            );
            self.content_items = vec![json!({"type": "inputText", "text": self.output.clone()})];
        }

        self
    }
}

pub trait DynamicToolExecutor: Send + Sync {
    fn tool_specs(&self) -> Vec<DynamicToolSpec>;
    fn execute<'a>(&'a self, tool: &'a str, arguments: Value) -> ToolFuture<'a>;
}

#[derive(Debug, Clone)]
pub struct LinearGraphqlTool {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
}

impl LinearGraphqlTool {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, DynamicToolError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(DynamicToolError::HttpClient)?;

        Ok(Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            client,
        })
    }

    pub fn spec() -> DynamicToolSpec {
        DynamicToolSpec {
            name: "linear_graphql".to_owned(),
            description: "Execute a raw GraphQL query or mutation against Linear using Symphony's configured auth.".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": {"type": "string"},
                    "variables": {"type": ["object", "null"], "additionalProperties": true}
                }
            }),
        }
    }

    pub async fn execute(&self, arguments: Value) -> DynamicToolResult {
        match normalize_arguments(arguments) {
            Ok((query, variables)) => {
                let request = json!({
                    "query": query,
                    "variables": variables,
                });
                let response = self
                    .client
                    .post(&self.endpoint)
                    .header(AUTHORIZATION, self.api_key.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .json(&request)
                    .send()
                    .await;

                match response {
                    Ok(response) => match response.json::<Value>().await {
                        Ok(payload) => {
                            let output = serde_json::to_string(&payload)
                                .unwrap_or_else(|_| "{\"error\":\"serialize\"}".to_owned());
                            let success = payload
                                .get("errors")
                                .and_then(Value::as_array)
                                .is_none_or(|errors| errors.is_empty());
                            if success {
                                DynamicToolResult::success(output)
                            } else {
                                DynamicToolResult::failure(output)
                            }
                        }
                        Err(error) => DynamicToolResult::failure(
                            json!({"error": {"message": "Linear GraphQL response decode failed.", "reason": error.to_string()}}).to_string(),
                        ),
                    },
                    Err(error) => DynamicToolResult::failure(
                        json!({"error": {"message": "Linear GraphQL request failed before receiving a successful response.", "reason": error.to_string()}}).to_string(),
                    ),
                }
            }
            Err(error) => DynamicToolResult::failure(error_payload(error).to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NonInteractiveDynamicToolExecutor {
    linear_graphql: Option<LinearGraphqlTool>,
}

impl NonInteractiveDynamicToolExecutor {
    pub fn new(config: TrackerConfig) -> Self {
        let linear_graphql = match (config.kind.as_deref(), config.api_key.as_deref()) {
            (Some("linear"), Some(api_key)) if !api_key.trim().is_empty() => {
                LinearGraphqlTool::new(config.endpoint, api_key.to_owned()).ok()
            }
            _ => None,
        };

        Self { linear_graphql }
    }
}

impl DynamicToolExecutor for NonInteractiveDynamicToolExecutor {
    fn tool_specs(&self) -> Vec<DynamicToolSpec> {
        self.linear_graphql
            .as_ref()
            .map(|_| vec![LinearGraphqlTool::spec()])
            .unwrap_or_default()
    }

    fn execute<'a>(&'a self, tool: &'a str, arguments: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            match (tool, &self.linear_graphql) {
                ("linear_graphql", Some(tool_impl)) => tool_impl.execute(arguments).await,
                _ => DynamicToolResult::unsupported(Some(tool), &self.tool_specs()),
            }
        })
    }
}

#[derive(Debug, Error)]
pub enum DynamicToolError {
    #[error("failed to build HTTP client: {0}")]
    HttpClient(reqwest::Error),
}

fn normalize_arguments(arguments: Value) -> Result<(String, Value), ToolArgumentError> {
    match arguments {
        Value::String(query) => {
            let query = query.trim().to_owned();
            if query.is_empty() {
                Err(ToolArgumentError::MissingQuery)
            } else {
                Ok((query, json!({})))
            }
        }
        Value::Object(object) => {
            let query = object
                .get("query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|query| !query.is_empty())
                .ok_or(ToolArgumentError::MissingQuery)?
                .to_owned();

            let variables = object
                .get("variables")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if variables.is_object() || variables.is_null() {
                Ok((
                    query,
                    if variables.is_null() {
                        json!({})
                    } else {
                        variables
                    },
                ))
            } else {
                Err(ToolArgumentError::InvalidVariables)
            }
        }
        _ => Err(ToolArgumentError::InvalidArguments),
    }
}

#[derive(Debug, Clone, Copy)]
enum ToolArgumentError {
    MissingQuery,
    InvalidArguments,
    InvalidVariables,
}

fn error_payload(error: ToolArgumentError) -> Value {
    match error {
        ToolArgumentError::MissingQuery => {
            json!({"error": {"message": "`linear_graphql` requires a non-empty `query` string."}})
        }
        ToolArgumentError::InvalidArguments => json!({
            "error": {
                "message": "`linear_graphql` expects either a GraphQL query string or an object with `query` and optional `variables`."
            }
        }),
        ToolArgumentError::InvalidVariables => json!({
            "error": {
                "message": "`linear_graphql.variables` must be a JSON object when provided."
            }
        }),
    }
}
