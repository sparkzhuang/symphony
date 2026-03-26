use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable tracker-internal issue identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssueId(String);

impl IssueId {
    /// Creates a new issue identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the underlying identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Human-readable issue key used in logs, prompts, and workspace naming.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssueIdentifier(String);

impl IssueIdentifier {
    /// Creates a new human-readable issue identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the underlying identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Sanitized workspace directory name derived from an issue identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceKey(String);

impl WorkspaceKey {
    /// Creates a new workspace key from an already sanitized string.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Derives a workspace key by replacing non `[A-Za-z0-9._-]` characters with `_`.
    pub fn from_issue_identifier(identifier: &str) -> Self {
        let sanitized = identifier
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();

        Self(sanitized)
    }

    /// Returns the underlying key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Composite coding-agent session identifier in `<thread_id>-<turn_id>` format.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Creates a session identifier from thread and turn ids.
    pub fn from_parts(thread_id: &str, turn_id: &str) -> Self {
        Self(format!("{thread_id}-{turn_id}"))
    }

    /// Returns the underlying session identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Normalized issue dependency reference used for blocker checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRef {
    /// Stable tracker id for the blocking issue when available.
    pub id: Option<IssueId>,
    /// Human-readable blocking issue identifier when available.
    pub identifier: Option<IssueIdentifier>,
    /// Current tracker state name for the blocking issue.
    pub state: Option<String>,
}

/// Parsed `WORKFLOW.md` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    /// Front matter root object preserved as a generic JSON value.
    pub config: Value,
    /// Trimmed Markdown prompt body rendered for agent runs.
    pub prompt_template: String,
}

impl WorkflowDefinition {
    /// Creates a workflow definition from raw config and prompt data.
    pub fn new(config: Value, prompt_template: impl Into<String>) -> Self {
        Self {
            config,
            prompt_template: prompt_template.into().trim().to_owned(),
        }
    }
}

/// Typed runtime values derived from workflow config plus environment resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Effective poll interval applied by the orchestrator.
    pub poll_interval_ms: u64,
    /// Root directory under which issue workspaces are created.
    pub workspace_root: PathBuf,
    /// Tracker states considered dispatch-eligible.
    pub active_states: Vec<String>,
    /// Tracker states treated as terminal by reconciliation and cleanup.
    pub terminal_states: Vec<String>,
    /// Global concurrency limit for agent sessions.
    pub max_concurrent_agents: usize,
    /// Coding-agent executable path or command name.
    pub agent_executable: String,
    /// Additional arguments passed to the coding-agent executable.
    pub agent_args: Vec<String>,
    /// Optional per-run timeout for agent execution.
    pub agent_timeout_ms: Option<u64>,
    /// Workspace lifecycle hooks resolved from workflow config.
    pub workspace_hooks: WorkspaceHooks,
}

/// Workspace hook configuration applied around workspace creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceHooks {
    /// Hook run before workspace preparation begins.
    pub before_create: Option<String>,
    /// Hook run after a workspace is created for the first time.
    pub after_create: Option<String>,
    /// Hook run before an existing workspace is reused.
    pub before_reuse: Option<String>,
    /// Hook run before a workspace is deleted during terminal cleanup.
    pub before_delete: Option<String>,
}

/// Filesystem workspace assigned to one issue identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Filesystem path used for the issue workspace.
    pub path: PathBuf,
    /// Sanitized directory key derived from the issue identifier.
    pub workspace_key: WorkspaceKey,
    /// Indicates whether the workspace was created during the current operation.
    pub created_now: bool,
}

impl Workspace {
    /// Creates a logical workspace from an issue identifier.
    pub fn for_issue_identifier(identifier: &str, created_now: bool) -> Self {
        let workspace_key = WorkspaceKey::from_issue_identifier(identifier);

        Self {
            path: PathBuf::from(workspace_key.as_str()),
            workspace_key,
            created_now,
        }
    }
}

/// One execution attempt for one issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAttempt {
    /// Stable tracker id of the issue being processed.
    pub issue_id: IssueId,
    /// Human-readable issue identifier used in logs and prompts.
    pub issue_identifier: IssueIdentifier,
    /// Retry attempt number; `None` represents the first execution.
    pub attempt: Option<u32>,
    /// Workspace path used for this attempt.
    pub workspace_path: PathBuf,
    /// Timestamp when the attempt started.
    pub started_at: DateTime<Utc>,
    /// Current lifecycle phase or terminal result of the run.
    pub status: RunStatus,
    /// Optional error detail captured for failure reporting and retries.
    pub error: Option<String>,
}

/// Run lifecycle phases and terminal outcomes defined by the specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    /// Workspace preparation is in progress.
    PreparingWorkspace,
    /// Prompt assembly is in progress.
    BuildingPrompt,
    /// The coding-agent process is being launched.
    LaunchingAgentProcess,
    /// Session metadata is being initialized from startup events.
    InitializingSession,
    /// An active coding-agent turn is streaming updates.
    StreamingTurn,
    /// The worker is performing shutdown and result handoff work.
    Finishing,
    /// The attempt completed successfully.
    Succeeded,
    /// The attempt failed unexpectedly.
    Failed,
    /// The attempt exceeded its configured timeout.
    TimedOut,
    /// The attempt was considered stalled and restarted.
    Stalled,
    /// The attempt was canceled because reconciliation made the issue ineligible.
    CanceledByReconciliation,
}

/// State tracked while a coding-agent subprocess is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSession {
    /// Composite session id `<thread_id>-<turn_id>`.
    pub session_id: SessionId,
    /// Coding-agent thread id.
    pub thread_id: String,
    /// Coding-agent turn id.
    pub turn_id: String,
    /// Operating-system process id reported for the app-server process.
    pub codex_app_server_pid: Option<String>,
    /// Most recent event name reported by the coding agent.
    pub last_codex_event: Option<String>,
    /// Timestamp of the most recent coding-agent event.
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    /// Summarized payload from the most recent coding-agent message.
    pub last_codex_message: Option<String>,
    /// Aggregate input tokens observed for the session.
    pub codex_input_tokens: u64,
    /// Aggregate output tokens observed for the session.
    pub codex_output_tokens: u64,
    /// Aggregate total tokens observed for the session.
    pub codex_total_tokens: u64,
    /// Last input-token total reported by the coding agent.
    pub last_reported_input_tokens: u64,
    /// Last output-token total reported by the coding agent.
    pub last_reported_output_tokens: u64,
    /// Last total-token count reported by the coding agent.
    pub last_reported_total_tokens: u64,
    /// Number of turns started within the current worker lifetime.
    pub turn_count: u64,
}

impl LiveSession {
    /// Creates a new empty live-session record from thread and turn ids.
    pub fn new(thread_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        let thread_id = thread_id.into();
        let turn_id = turn_id.into();

        Self {
            session_id: SessionId::from_parts(&thread_id, &turn_id),
            thread_id,
            turn_id,
            codex_app_server_pid: None,
            last_codex_event: None,
            last_codex_timestamp: None,
            last_codex_message: None,
            codex_input_tokens: 0,
            codex_output_tokens: 0,
            codex_total_tokens: 0,
            last_reported_input_tokens: 0,
            last_reported_output_tokens: 0,
            last_reported_total_tokens: 0,
            turn_count: 0,
        }
    }
}

/// Scheduled retry state for an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryEntry {
    /// Stable tracker id of the issue awaiting retry.
    pub issue_id: IssueId,
    /// Human-readable issue identifier for logs and status surfaces.
    pub identifier: IssueIdentifier,
    /// One-based retry attempt number.
    pub attempt: u32,
    /// Monotonic-clock due time expressed in milliseconds.
    pub due_at_ms: u64,
    /// Runtime-specific timer reference captured as a string when available.
    pub timer_handle: Option<String>,
    /// Last error associated with the retry schedule.
    pub error: Option<String>,
}

/// Aggregate coding-agent token and runtime totals across the service lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CodexTotals {
    /// Aggregate input token count across completed sessions.
    pub input_tokens: u64,
    /// Aggregate output token count across completed sessions.
    pub output_tokens: u64,
    /// Aggregate total token count across completed sessions.
    pub total_tokens: u64,
    /// Aggregate wall-clock runtime captured in seconds.
    pub seconds_running: u64,
}

/// Runtime view of a single claimed/running issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningEntry {
    /// Latest normalized issue snapshot associated with the run.
    pub issue: Issue,
    /// Active attempt metadata for the worker.
    pub run_attempt: RunAttempt,
    /// Live coding-agent session metadata when available.
    pub live_session: Option<LiveSession>,
    /// Hostname or worker identity executing the run.
    pub worker_host: Option<String>,
}

/// Single authoritative in-memory state owned by the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorState {
    /// Current effective polling cadence.
    pub poll_interval_ms: u64,
    /// Current effective global concurrency limit.
    pub max_concurrent_agents: usize,
    /// Running issue map keyed by stable issue id.
    pub running: HashMap<IssueId, RunningEntry>,
    /// Claimed issue ids reserved, running, or queued for retry.
    pub claimed: HashSet<IssueId>,
    /// Retry queue entries keyed by stable issue id.
    pub retry_attempts: HashMap<IssueId, RetryEntry>,
    /// Bookkeeping set of issue ids completed during the current process lifetime.
    pub completed: HashSet<IssueId>,
    /// Aggregate coding-agent usage totals.
    pub codex_totals: CodexTotals,
    /// Latest rate-limit snapshot reported by coding-agent events.
    pub codex_rate_limits: Option<Value>,
}

/// Normalized issue record used by orchestration, prompt rendering, and observability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Stable tracker-internal issue id.
    pub id: IssueId,
    /// Human-readable ticket key such as `SPA-8`.
    pub identifier: IssueIdentifier,
    /// Issue title rendered in prompts and status output.
    pub title: String,
    /// Optional issue description body.
    pub description: Option<String>,
    /// Tracker priority where lower numbers represent higher priority.
    pub priority: Option<u8>,
    /// Current tracker state name.
    pub state: String,
    /// Tracker-provided branch metadata when available.
    pub branch_name: Option<String>,
    /// Canonical tracker URL when available.
    pub url: Option<String>,
    /// Normalized lowercase label names associated with the issue.
    pub labels: Vec<String>,
    /// Blocking issues that gate dispatch for certain states.
    pub blocked_by: Vec<BlockerRef>,
    /// Tracker creation timestamp when available.
    pub created_at: Option<DateTime<Utc>>,
    /// Tracker update timestamp when available.
    pub updated_at: Option<DateTime<Utc>>,
}
