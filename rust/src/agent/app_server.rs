use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::warn;

use crate::agent::protocol::{
    rate_limits_from_event, thread_id_from_response, tool_name, turn_id_from_response,
    usage_from_event, FIRST_TURN_START_ID, INITIALIZE_ID, MAX_LINE_BYTES, THREAD_START_ID,
};
use crate::agent::tools::{DynamicToolExecutor, NonInteractiveDynamicToolExecutor};
use crate::config::{CodexConfig, TrackerConfig};
use crate::types::{Issue, SessionId};

const NON_INTERACTIVE_ANSWER: &str =
    "This is a non-interactive session. Operator input is unavailable.";

type EventSink = Arc<dyn Fn(AppServerEvent) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppServerEventKind {
    SessionStarted,
    StartupFailed,
    ApprovalResolved,
    ToolInputResolved,
    UnsupportedToolCall,
    ToolCallFailed,
    TurnCompleted,
    TurnFailed,
    TurnCancelled,
    Malformed,
    Notification,
    OtherMessage,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppServerEvent {
    pub kind: AppServerEventKind,
    pub timestamp: chrono::DateTime<Utc>,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerConfig {
    pub command: String,
    pub approval_policy: Option<Value>,
    pub thread_sandbox: Option<String>,
    pub turn_sandbox_policy: Option<Value>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
}

impl AppServerConfig {
    pub fn from_codex_config(config: &CodexConfig) -> Self {
        Self {
            command: config.command.clone(),
            approval_policy: config.approval_policy.clone(),
            thread_sandbox: config.thread_sandbox.clone(),
            turn_sandbox_policy: config.turn_sandbox_policy.clone(),
            turn_timeout_ms: config.turn_timeout_ms,
            read_timeout_ms: config.read_timeout_ms,
        }
    }
}

pub struct AppServerSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_task: Option<JoinHandle<()>>,
    pending_line: String,
    pub thread_id: String,
    pub workspace: PathBuf,
    next_turn_request_id: u64,
    sink: Option<EventSink>,
    pid: Option<String>,
    tool_executor: Arc<dyn DynamicToolExecutor>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub session_id: SessionId,
    pub thread_id: String,
    pub turn_id: String,
    pub usage: Option<Value>,
    pub rate_limits: Option<Value>,
}

pub struct AppServerClient {
    config: AppServerConfig,
    tracker: TrackerConfig,
    tool_executor: Arc<dyn DynamicToolExecutor>,
}

impl std::fmt::Debug for AppServerClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppServerClient")
            .field("config", &self.config)
            .field("tracker", &self.tracker)
            .finish_non_exhaustive()
    }
}

impl AppServerClient {
    pub fn new<E>(config: AppServerConfig, tracker: TrackerConfig, tool_executor: E) -> Self
    where
        E: DynamicToolExecutor + 'static,
    {
        Self {
            config,
            tracker,
            tool_executor: Arc::new(tool_executor),
        }
    }

    pub async fn start_session(
        &mut self,
        workspace: &Path,
        sink: Option<EventSink>,
    ) -> Result<AppServerSession, AppServerError> {
        let workspace = absolute_workspace_path(workspace)?;
        let mut command = Command::new("bash");
        command
            .arg("-lc")
            .arg(&self.config.command)
            .current_dir(&workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|source| AppServerError::Spawn {
            command: self.config.command.clone(),
            source,
        })?;
        let pid = child.id().map(|pid| pid.to_string());
        let stdin = child
            .stdin
            .take()
            .ok_or(AppServerError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(AppServerError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(AppServerError::MissingPipe("stderr"))?;
        let stderr_task = spawn_stderr_logger(stderr);
        let mut session = AppServerSession {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_task: Some(stderr_task),
            pending_line: String::new(),
            thread_id: String::new(),
            workspace: workspace.clone(),
            next_turn_request_id: FIRST_TURN_START_ID,
            sink,
            pid,
            tool_executor: Arc::clone(&self.tool_executor),
        };

        self.send_json(
            &mut session,
            &json!({
                "id": INITIALIZE_ID,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "symphony-rust",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "experimentalApi": true,
                    }
                }
            }),
        )
        .await?;
        self.await_response(&mut session, INITIALIZE_ID).await?;

        self.send_json(
            &mut session,
            &json!({
                "method": "initialized",
                "params": {},
            }),
        )
        .await?;
        let mut params = json!({
            "approvalPolicy": self.config.approval_policy.clone().unwrap_or(Value::Null),
            "cwd": workspace,
            "dynamicTools": self.tool_executor.tool_specs(),
        });
        if let Some(sandbox) = &self.config.thread_sandbox {
            params["sandbox"] = json!(sandbox);
        }
        self.send_json(
            &mut session,
            &json!({
                "id": THREAD_START_ID,
                "method": "thread/start",
                "params": params,
            }),
        )
        .await?;
        let response = self.await_response(&mut session, THREAD_START_ID).await?;
        session.thread_id = thread_id_from_response(&response)
            .ok_or_else(|| AppServerError::InvalidResponse("missing thread id".to_owned()))?;

        Ok(session)
    }

    pub async fn run_turn(
        &mut self,
        session: &mut AppServerSession,
        prompt: &str,
        issue: &Issue,
    ) -> Result<TurnOutcome, AppServerError> {
        let request_id = session.next_turn_request_id;
        session.next_turn_request_id += 1;

        let mut params = json!({
            "threadId": session.thread_id,
            "input": [{"type": "text", "text": prompt}],
            "cwd": session.workspace,
            "title": format!("{}: {}", issue.identifier.as_str(), issue.title),
            "approvalPolicy": self.config.approval_policy.clone().unwrap_or(Value::Null),
        });
        if let Some(sandbox_policy) = &self.config.turn_sandbox_policy {
            params["sandboxPolicy"] = sandbox_policy.clone();
        }

        self.send_json(
            session,
            &json!({
                "id": request_id,
                "method": "turn/start",
                "params": params,
            }),
        )
        .await?;

        let response = self.await_response(session, request_id).await?;
        let turn_id = turn_id_from_response(&response)
            .ok_or_else(|| AppServerError::InvalidResponse("missing turn id".to_owned()))?;
        let outcome = TurnOutcome {
            session_id: SessionId::from_parts(&session.thread_id, &turn_id),
            thread_id: session.thread_id.clone(),
            turn_id,
            usage: None,
            rate_limits: None,
        };

        emit(
            &session.sink,
            AppServerEventKind::SessionStarted,
            json!({
                "session_id": outcome.session_id.as_str(),
                "thread_id": outcome.thread_id,
                "turn_id": outcome.turn_id,
                "codex_app_server_pid": session.pid,
            }),
        );

        self.stream_turn(session, outcome).await
    }

    pub async fn stop_session(
        &mut self,
        session: &mut AppServerSession,
    ) -> Result<(), AppServerError> {
        if session.child.try_wait()?.is_none() {
            let _ = session.child.kill().await;
        }
        if let Some(stderr_task) = session.stderr_task.take() {
            let _ = stderr_task.await;
        }
        Ok(())
    }

    async fn stream_turn(
        &mut self,
        session: &mut AppServerSession,
        mut outcome: TurnOutcome,
    ) -> Result<TurnOutcome, AppServerError> {
        let deadline = Instant::now() + Duration::from_millis(self.config.turn_timeout_ms);
        loop {
            let message = self.read_next_turn_json(session, deadline).await?;
            let Some(method) = message.get("method").and_then(Value::as_str) else {
                continue;
            };

            match method {
                "turn/completed" => {
                    outcome.usage = usage_from_event(&message);
                    outcome.rate_limits = rate_limits_from_event(&message);
                    emit(&session.sink, AppServerEventKind::TurnCompleted, message);
                    return Ok(outcome);
                }
                "turn/failed" => {
                    emit(
                        &session.sink,
                        AppServerEventKind::TurnFailed,
                        message.clone(),
                    );
                    return Err(AppServerError::TurnFailed(message));
                }
                "turn/cancelled" => {
                    emit(
                        &session.sink,
                        AppServerEventKind::TurnCancelled,
                        message.clone(),
                    );
                    return Err(AppServerError::TurnCancelled(message));
                }
                "turn/input_required" => {
                    return Err(AppServerError::TurnInputRequired(message));
                }
                request if request.ends_with("requestApproval") => {
                    let decision = if approval_policy_is_never(self.config.approval_policy.as_ref())
                    {
                        "acceptForSession"
                    } else {
                        "deny"
                    };
                    self.send_json(
                        session,
                        &json!({
                            "id": message.get("id").cloned().unwrap_or(Value::Null),
                            "result": {"decision": decision},
                        }),
                    )
                    .await?;
                    emit(&session.sink, AppServerEventKind::ApprovalResolved, message);
                }
                "item/tool/requestUserInput" => {
                    let answers = tool_input_answers(
                        message.get("params"),
                        approval_policy_is_never(self.config.approval_policy.as_ref()),
                    );

                    self.send_json(
                        session,
                        &json!({
                            "id": message.get("id").cloned().unwrap_or(Value::Null),
                            "result": {"answers": answers},
                        }),
                    )
                    .await?;
                    emit(
                        &session.sink,
                        AppServerEventKind::ToolInputResolved,
                        json!({"request": message, "answer": NON_INTERACTIVE_ANSWER}),
                    );
                }
                "item/tool/call" => {
                    let name = tool_name(&message).unwrap_or("unknown");
                    let arguments = message
                        .get("params")
                        .and_then(|params| params.get("arguments"))
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let result = session.tool_executor.execute(name, arguments).await;
                    self.send_json(
                        session,
                        &json!({
                            "id": message.get("id").cloned().unwrap_or(Value::Null),
                            "result": serde_json::to_value(&result).unwrap_or_else(|_| json!({"success": false, "output": "{\"error\":\"serialization\"}", "contentItems": []})),
                        }),
                    )
                    .await?;

                    if result.success {
                        emit(&session.sink, AppServerEventKind::Notification, message);
                    } else if self
                        .tool_executor
                        .tool_specs()
                        .iter()
                        .all(|spec| spec.name != name)
                    {
                        emit(
                            &session.sink,
                            AppServerEventKind::UnsupportedToolCall,
                            message,
                        );
                    } else {
                        emit(&session.sink, AppServerEventKind::ToolCallFailed, message);
                    }
                }
                _ => emit(&session.sink, AppServerEventKind::Notification, message),
            }
        }
    }

    async fn read_next_turn_json(
        &mut self,
        session: &mut AppServerSession,
        deadline: Instant,
    ) -> Result<Value, AppServerError> {
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(AppServerError::TurnTimeout(self.config.turn_timeout_ms));
            };
            let line = timeout(remaining, read_complete_line(session))
                .await
                .map_err(|_| AppServerError::TurnTimeout(self.config.turn_timeout_ms))??;
            let Some(line) = line else {
                return Err(AppServerError::PortExit);
            };
            if line.is_empty() {
                continue;
            }

            match serde_json::from_str::<Value>(&line) {
                Ok(value) => return Ok(value),
                Err(_) => {
                    if line.trim_start().starts_with('{') {
                        emit(&session.sink, AppServerEventKind::Malformed, json!(line));
                    }
                }
            }
        }
    }

    async fn await_response(
        &mut self,
        session: &mut AppServerSession,
        id: u64,
    ) -> Result<Value, AppServerError> {
        loop {
            let message = self
                .read_next_json(session, self.config.read_timeout_ms)
                .await?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if message.get("error").is_some() {
                    return Err(AppServerError::ResponseError(message));
                }
                return Ok(message);
            }

            emit(&session.sink, AppServerEventKind::OtherMessage, message);
        }
    }

    async fn read_next_json(
        &mut self,
        session: &mut AppServerSession,
        timeout_ms: u64,
    ) -> Result<Value, AppServerError> {
        loop {
            let line = timeout(
                Duration::from_millis(timeout_ms),
                read_complete_line(session),
            )
            .await
            .map_err(|_| AppServerError::ResponseTimeout(timeout_ms))??;
            let Some(line) = line else {
                return Err(AppServerError::PortExit);
            };
            if line.is_empty() {
                continue;
            }

            match serde_json::from_str::<Value>(&line) {
                Ok(value) => return Ok(value),
                Err(_) => {
                    if line.trim_start().starts_with('{') {
                        emit(&session.sink, AppServerEventKind::Malformed, json!(line));
                    }
                }
            }
        }
    }

    async fn send_json(
        &mut self,
        session: &mut AppServerSession,
        payload: &Value,
    ) -> Result<(), AppServerError> {
        let encoded = serde_json::to_string(payload)
            .map_err(|error| AppServerError::InvalidResponse(error.to_string()))?;
        session.stdin.write_all(encoded.as_bytes()).await?;
        session.stdin.write_all(b"\n").await?;
        session.stdin.flush().await?;
        Ok(())
    }
}

impl Default for AppServerClient {
    fn default() -> Self {
        let tracker = TrackerConfig::default();
        let executor = NonInteractiveDynamicToolExecutor::new(tracker.clone());
        Self::new(
            AppServerConfig::from_codex_config(&CodexConfig::default()),
            tracker,
            executor,
        )
    }
}

#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("failed to spawn `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("missing child pipe: {0}")]
    MissingPipe(&'static str),
    #[error("request/response timed out after {0} ms")]
    ResponseTimeout(u64),
    #[error("turn timed out after {0} ms")]
    TurnTimeout(u64),
    #[error("codex app-server exited unexpectedly")]
    PortExit,
    #[error("response returned error payload: {0}")]
    ResponseError(Value),
    #[error("invalid protocol response: {0}")]
    InvalidResponse(String),
    #[error("turn failed: {0}")]
    TurnFailed(Value),
    #[error("turn cancelled: {0}")]
    TurnCancelled(Value),
    #[error("turn input required: {0}")]
    TurnInputRequired(Value),
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
}

async fn read_complete_line(
    session: &mut AppServerSession,
) -> Result<Option<String>, AppServerError> {
    loop {
        let mut line = String::new();
        let read = session.stdout.read_line(&mut line).await?;
        if read == 0 {
            if session.pending_line.is_empty() {
                return Ok(None);
            }

            let pending = std::mem::take(&mut session.pending_line);
            return Ok(Some(pending));
        }

        if session.pending_line.len() + line.len() > MAX_LINE_BYTES {
            return Err(AppServerError::InvalidResponse(
                "protocol line exceeded 10MB".to_owned(),
            ));
        }

        session.pending_line.push_str(&line);
        if session.pending_line.ends_with('\n') {
            let complete = session
                .pending_line
                .trim_end_matches(['\n', '\r'])
                .to_owned();
            session.pending_line.clear();
            return Ok(Some(complete));
        }
    }
}

fn spawn_stderr_logger(stderr: ChildStderr) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => warn!("Codex turn stream output: {}", line.trim_end()),
                Err(error) => {
                    warn!("Failed reading Codex stderr: {error}");
                    break;
                }
            }
        }
    })
}

fn emit(sink: &Option<EventSink>, kind: AppServerEventKind, payload: Value) {
    if let Some(sink) = sink {
        sink(AppServerEvent {
            kind,
            timestamp: Utc::now(),
            payload,
        });
    }
}

fn approval_policy_is_never(policy: Option<&Value>) -> bool {
    matches!(policy, Some(Value::String(value)) if value == "never")
}

fn tool_input_answers(params: Option<&Value>, auto_approve: bool) -> Value {
    let Some(questions) = params
        .and_then(|params| params.get("questions"))
        .and_then(Value::as_array)
    else {
        return json!({});
    };

    let mut answers = serde_json::Map::new();
    for question in questions {
        let Some(id) = question.get("id").and_then(Value::as_str) else {
            continue;
        };
        let answer = if auto_approve {
            tool_approval_answer(question).unwrap_or(NON_INTERACTIVE_ANSWER)
        } else {
            NON_INTERACTIVE_ANSWER
        };
        answers.insert(id.to_owned(), json!({ "answers": [answer] }));
    }

    Value::Object(answers)
}

fn tool_approval_answer(question: &Value) -> Option<&'static str> {
    let options = question.get("options")?.as_array()?;
    if options
        .iter()
        .any(|option| option.get("label").and_then(Value::as_str) == Some("Approve this Session"))
    {
        Some("Approve this Session")
    } else {
        None
    }
}

fn absolute_workspace_path(workspace: &Path) -> Result<PathBuf, AppServerError> {
    if workspace.is_absolute() {
        Ok(workspace.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(workspace))
    }
}
