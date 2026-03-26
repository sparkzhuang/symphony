use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use dirs::home_dir;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";
const DEFAULT_ACTIVE_STATES: [&str; 2] = ["Todo", "In Progress"];
const DEFAULT_TERMINAL_STATES: [&str; 5] = ["Closed", "Cancelled", "Canceled", "Duplicate", "Done"];
const DEFAULT_POLL_INTERVAL_MS: u64 = 30_000;
const DEFAULT_WORKSPACE_DIR: &str = "symphony_workspaces";
const DEFAULT_HOOK_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_CONCURRENT_AGENTS: usize = 10;
const DEFAULT_MAX_TURNS: u32 = 20;
const DEFAULT_MAX_RETRY_BACKOFF_MS: u64 = 300_000;
const DEFAULT_CODEX_COMMAND: &str = "codex app-server";
const DEFAULT_THREAD_SANDBOX: &str = "workspace-write";
const DEFAULT_TURN_TIMEOUT_MS: u64 = 3_600_000;
const DEFAULT_READ_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_STALL_TIMEOUT_MS: u64 = 300_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowConfig {
    pub tracker: TrackerConfig,
    pub polling: PollingConfig,
    pub workspace: WorkspaceConfig,
    pub hooks: HooksConfig,
    pub agent: AgentConfig,
    pub codex: CodexConfig,
    pub server: ServerConfig,
}

impl WorkflowConfig {
    pub fn from_value(value: Value) -> Result<Self, ConfigError> {
        let Some(root) = value.as_object() else {
            return Err(ConfigError::InvalidConfig(vec![ValidationError::new(
                "config",
                ConfigValueError::ExpectedObject,
            )]));
        };

        let tracker_value = object_field(root, "tracker");
        let polling_value = object_field(root, "polling");
        let workspace_value = object_field(root, "workspace");
        let hooks_value = object_field(root, "hooks");
        let agent_value = object_field(root, "agent");
        let codex_value = object_field(root, "codex");
        let server_value = object_field(root, "server");

        let mut errors = Vec::new();
        let tracker = TrackerConfig::parse(tracker_value, &mut errors);
        let polling = PollingConfig::parse(polling_value, &mut errors);
        let workspace = WorkspaceConfig::parse(workspace_value, &mut errors);
        let hooks = HooksConfig::parse(hooks_value, &mut errors);
        let agent = AgentConfig::parse(agent_value, &mut errors);
        let codex = CodexConfig::parse(codex_value, &mut errors);
        let server = ServerConfig::parse(server_value, &mut errors);

        if errors.is_empty() {
            Ok(Self {
                tracker,
                polling,
                workspace,
                hooks,
                agent,
                codex,
                server,
            })
        } else {
            Err(ConfigError::InvalidConfig(errors))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerConfig {
    pub kind: Option<String>,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub project_slug: Option<String>,
    pub assignee: Option<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

impl TrackerConfig {
    fn parse(value: Option<&Value>, errors: &mut Vec<ValidationError>) -> Self {
        let Some(object) = expect_optional_object(value, "tracker", errors) else {
            return Self::default();
        };

        Self {
            kind: optional_trimmed_string(object.get("kind"), "tracker.kind", errors),
            endpoint: optional_trimmed_string(object.get("endpoint"), "tracker.endpoint", errors)
                .unwrap_or_else(|| DEFAULT_LINEAR_ENDPOINT.to_owned()),
            api_key: resolve_secret(
                object.get("api_key"),
                "tracker.api_key",
                Some("LINEAR_API_KEY"),
                errors,
            ),
            project_slug: optional_trimmed_string(
                object.get("project_slug"),
                "tracker.project_slug",
                errors,
            ),
            assignee: resolve_secret(
                object.get("assignee"),
                "tracker.assignee",
                Some("LINEAR_ASSIGNEE"),
                errors,
            ),
            active_states: string_list(
                object.get("active_states"),
                "tracker.active_states",
                &DEFAULT_ACTIVE_STATES,
                errors,
            ),
            terminal_states: string_list(
                object.get("terminal_states"),
                "tracker.terminal_states",
                &DEFAULT_TERMINAL_STATES,
                errors,
            ),
        }
    }
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            kind: None,
            endpoint: DEFAULT_LINEAR_ENDPOINT.to_owned(),
            api_key: env::var("LINEAR_API_KEY")
                .ok()
                .and_then(|value| normalize_secret_value(Some(value))),
            project_slug: None,
            assignee: env::var("LINEAR_ASSIGNEE")
                .ok()
                .and_then(|value| normalize_secret_value(Some(value))),
            active_states: DEFAULT_ACTIVE_STATES
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            terminal_states: DEFAULT_TERMINAL_STATES
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollingConfig {
    pub interval_ms: u64,
}

impl PollingConfig {
    fn parse(value: Option<&Value>, errors: &mut Vec<ValidationError>) -> Self {
        let Some(object) = expect_optional_object(value, "polling", errors) else {
            return Self::default();
        };

        let interval_ms = positive_u64(
            object.get("interval_ms"),
            "polling.interval_ms",
            Some(DEFAULT_POLL_INTERVAL_MS),
            errors,
        )
        .unwrap_or(DEFAULT_POLL_INTERVAL_MS);

        Self { interval_ms }
    }
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            interval_ms: DEFAULT_POLL_INTERVAL_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
}

impl WorkspaceConfig {
    fn parse(value: Option<&Value>, errors: &mut Vec<ValidationError>) -> Self {
        let Some(object) = expect_optional_object(value, "workspace", errors) else {
            return Self::default();
        };

        let root = resolve_path(
            object.get("root"),
            "workspace.root",
            default_workspace_root(),
            errors,
        );

        Self { root }
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: default_workspace_root(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksConfig {
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    pub timeout_ms: u64,
}

impl HooksConfig {
    fn parse(value: Option<&Value>, errors: &mut Vec<ValidationError>) -> Self {
        let Some(object) = expect_optional_object(value, "hooks", errors) else {
            return Self::default();
        };

        let timeout_ms = match positive_u64(
            object.get("timeout_ms"),
            "hooks.timeout_ms",
            Some(DEFAULT_HOOK_TIMEOUT_MS),
            errors,
        ) {
            Some(value) => value,
            None => DEFAULT_HOOK_TIMEOUT_MS,
        };

        Self {
            after_create: optional_trimmed_string(
                object.get("after_create"),
                "hooks.after_create",
                errors,
            ),
            before_run: optional_trimmed_string(
                object.get("before_run"),
                "hooks.before_run",
                errors,
            ),
            after_run: optional_trimmed_string(object.get("after_run"), "hooks.after_run", errors),
            before_remove: optional_trimmed_string(
                object.get("before_remove"),
                "hooks.before_remove",
                errors,
            ),
            timeout_ms,
        }
    }
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            after_create: None,
            before_run: None,
            after_run: None,
            before_remove: None,
            timeout_ms: DEFAULT_HOOK_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub max_concurrent_agents: usize,
    pub max_turns: u32,
    pub max_retry_backoff_ms: u64,
    pub max_concurrent_agents_by_state: HashMap<String, usize>,
}

impl AgentConfig {
    fn parse(value: Option<&Value>, errors: &mut Vec<ValidationError>) -> Self {
        let Some(object) = expect_optional_object(value, "agent", errors) else {
            return Self::default();
        };

        Self {
            max_concurrent_agents: positive_usize(
                object.get("max_concurrent_agents"),
                "agent.max_concurrent_agents",
                Some(DEFAULT_MAX_CONCURRENT_AGENTS),
                errors,
            )
            .unwrap_or(DEFAULT_MAX_CONCURRENT_AGENTS),
            max_turns: positive_u32(
                object.get("max_turns"),
                "agent.max_turns",
                Some(DEFAULT_MAX_TURNS),
                errors,
            )
            .unwrap_or(DEFAULT_MAX_TURNS),
            max_retry_backoff_ms: positive_u64(
                object.get("max_retry_backoff_ms"),
                "agent.max_retry_backoff_ms",
                Some(DEFAULT_MAX_RETRY_BACKOFF_MS),
                errors,
            )
            .unwrap_or(DEFAULT_MAX_RETRY_BACKOFF_MS),
            max_concurrent_agents_by_state: parse_state_limits(
                object.get("max_concurrent_agents_by_state"),
                "agent.max_concurrent_agents_by_state",
                errors,
            ),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_concurrent_agents: DEFAULT_MAX_CONCURRENT_AGENTS,
            max_turns: DEFAULT_MAX_TURNS,
            max_retry_backoff_ms: DEFAULT_MAX_RETRY_BACKOFF_MS,
            max_concurrent_agents_by_state: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexConfig {
    pub command: String,
    pub approval_policy: Option<Value>,
    pub thread_sandbox: Option<String>,
    pub turn_sandbox_policy: Option<Value>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: u64,
}

impl CodexConfig {
    fn parse(value: Option<&Value>, errors: &mut Vec<ValidationError>) -> Self {
        let Some(object) = expect_optional_object(value, "codex", errors) else {
            return Self::default();
        };

        let command = required_non_empty_string(
            object.get("command"),
            "codex.command",
            DEFAULT_CODEX_COMMAND,
            errors,
        );

        Self {
            command,
            approval_policy: object.get("approval_policy").cloned(),
            thread_sandbox: optional_trimmed_string(
                object.get("thread_sandbox"),
                "codex.thread_sandbox",
                errors,
            )
            .or_else(|| Some(DEFAULT_THREAD_SANDBOX.to_owned())),
            turn_sandbox_policy: object.get("turn_sandbox_policy").cloned(),
            turn_timeout_ms: positive_u64(
                object.get("turn_timeout_ms"),
                "codex.turn_timeout_ms",
                Some(DEFAULT_TURN_TIMEOUT_MS),
                errors,
            )
            .unwrap_or(DEFAULT_TURN_TIMEOUT_MS),
            read_timeout_ms: positive_u64(
                object.get("read_timeout_ms"),
                "codex.read_timeout_ms",
                Some(DEFAULT_READ_TIMEOUT_MS),
                errors,
            )
            .unwrap_or(DEFAULT_READ_TIMEOUT_MS),
            stall_timeout_ms: non_negative_u64(
                object.get("stall_timeout_ms"),
                "codex.stall_timeout_ms",
                Some(DEFAULT_STALL_TIMEOUT_MS),
                errors,
            )
            .unwrap_or(DEFAULT_STALL_TIMEOUT_MS),
        }
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            command: DEFAULT_CODEX_COMMAND.to_owned(),
            approval_policy: None,
            thread_sandbox: Some(DEFAULT_THREAD_SANDBOX.to_owned()),
            turn_sandbox_policy: None,
            turn_timeout_ms: DEFAULT_TURN_TIMEOUT_MS,
            read_timeout_ms: DEFAULT_READ_TIMEOUT_MS,
            stall_timeout_ms: DEFAULT_STALL_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    pub port: Option<u16>,
}

impl ServerConfig {
    fn parse(value: Option<&Value>, errors: &mut Vec<ValidationError>) -> Self {
        let Some(object) = expect_optional_object(value, "server", errors) else {
            return Self::default();
        };

        let port = optional_u16(object.get("port"), "server.port", errors);
        Self { port }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("invalid workflow config")]
    InvalidConfig(Vec<ValidationError>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub path: String,
    pub kind: ConfigValueError,
}

impl ValidationError {
    pub fn new(path: impl Into<String>, kind: ConfigValueError) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValueError {
    Required,
    ExpectedArray,
    ExpectedInteger,
    ExpectedObject,
    ExpectedString,
    MustBeNonNegative,
    MustBePositive,
    UnsupportedValue { expected: String, actual: String },
}

pub fn validate_dispatch_preflight(config: &WorkflowConfig) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    match config.tracker.kind.as_deref() {
        Some("linear") => {
            if config.tracker.api_key.is_none() {
                errors.push(ValidationError::new(
                    "tracker.api_key",
                    ConfigValueError::Required,
                ));
            }

            if config
                .tracker
                .project_slug
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(ValidationError::new(
                    "tracker.project_slug",
                    ConfigValueError::Required,
                ));
            }
        }
        Some(other) => errors.push(ValidationError::new(
            "tracker.kind",
            ConfigValueError::UnsupportedValue {
                expected: "linear".to_owned(),
                actual: other.to_owned(),
            },
        )),
        None => errors.push(ValidationError::new(
            "tracker.kind",
            ConfigValueError::Required,
        )),
    }

    if config.codex.command.trim().is_empty() {
        errors.push(ValidationError::new(
            "codex.command",
            ConfigValueError::Required,
        ));
    }

    errors
}

fn object_field<'a>(root: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    root.get(key)
}

fn expect_optional_object<'a>(
    value: Option<&'a Value>,
    path: &str,
    errors: &mut Vec<ValidationError>,
) -> Option<&'a Map<String, Value>> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::Object(object)) => Some(object),
        Some(_) => {
            errors.push(ValidationError::new(path, ConfigValueError::ExpectedObject));
            None
        }
    }
}

fn optional_trimmed_string(
    value: Option<&Value>,
    path: &str,
    errors: &mut Vec<ValidationError>,
) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        Some(_) => {
            errors.push(ValidationError::new(path, ConfigValueError::ExpectedString));
            None
        }
    }
}

fn required_non_empty_string(
    value: Option<&Value>,
    path: &str,
    default: &str,
    errors: &mut Vec<ValidationError>,
) -> String {
    match value {
        None | Some(Value::Null) => default.to_owned(),
        Some(Value::String(raw)) if raw.trim().is_empty() => {
            errors.push(ValidationError::new(path, ConfigValueError::Required));
            default.to_owned()
        }
        Some(Value::String(raw)) => raw.trim().to_owned(),
        Some(_) => {
            errors.push(ValidationError::new(path, ConfigValueError::ExpectedString));
            default.to_owned()
        }
    }
}

fn string_list(
    value: Option<&Value>,
    path: &str,
    default: &[&str],
    errors: &mut Vec<ValidationError>,
) -> Vec<String> {
    match value {
        None | Some(Value::Null) => default.iter().map(|item| (*item).to_owned()).collect(),
        Some(Value::Array(items)) => {
            let mut result = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                match item {
                    Value::String(value) => result.push(value.clone()),
                    _ => errors.push(ValidationError::new(
                        format!("{path}[{index}]"),
                        ConfigValueError::ExpectedString,
                    )),
                }
            }

            if result.is_empty() && !items.is_empty() {
                default.iter().map(|item| (*item).to_owned()).collect()
            } else {
                result
            }
        }
        Some(_) => {
            errors.push(ValidationError::new(path, ConfigValueError::ExpectedArray));
            default.iter().map(|item| (*item).to_owned()).collect()
        }
    }
}

fn parse_state_limits(
    value: Option<&Value>,
    path: &str,
    errors: &mut Vec<ValidationError>,
) -> HashMap<String, usize> {
    let Some(value) = value else {
        return HashMap::new();
    };

    let Some(object) = value.as_object() else {
        errors.push(ValidationError::new(path, ConfigValueError::ExpectedObject));
        return HashMap::new();
    };

    let mut limits = HashMap::new();
    for (state_name, raw_limit) in object {
        let trimmed = state_name.trim();
        let entry_path = format!("{path}.{state_name}");
        if trimmed.is_empty() {
            errors.push(ValidationError::new(entry_path, ConfigValueError::Required));
            continue;
        }

        match parse_integer(raw_limit) {
            Some(limit) if limit > 0 => {
                let normalized = trimmed.to_lowercase();
                limits.insert(normalized, limit as usize);
            }
            Some(_) => errors.push(ValidationError::new(
                entry_path,
                ConfigValueError::MustBePositive,
            )),
            None => errors.push(ValidationError::new(
                entry_path,
                ConfigValueError::ExpectedInteger,
            )),
        }
    }

    limits
}

fn resolve_secret(
    value: Option<&Value>,
    path: &str,
    fallback_env: Option<&str>,
    errors: &mut Vec<ValidationError>,
) -> Option<String> {
    match value {
        None | Some(Value::Null) => fallback_env.and_then(resolve_env_secret),
        Some(Value::String(raw)) => {
            let resolved = if let Some(name) = env_reference_name(raw) {
                resolve_env_secret(name).or_else(|| fallback_env.and_then(resolve_env_secret))
            } else {
                normalize_secret_value(Some(raw.to_owned()))
            };

            resolved
        }
        Some(_) => {
            errors.push(ValidationError::new(path, ConfigValueError::ExpectedString));
            fallback_env.and_then(resolve_env_secret)
        }
    }
}

fn resolve_path(
    value: Option<&Value>,
    path: &str,
    default: PathBuf,
    errors: &mut Vec<ValidationError>,
) -> PathBuf {
    let Some(value) = value else {
        return default;
    };

    let Some(raw) = value.as_str() else {
        errors.push(ValidationError::new(path, ConfigValueError::ExpectedString));
        return default;
    };

    let maybe_resolved = if let Some(name) = env_reference_name(raw) {
        env::var(name).ok()
    } else {
        Some(raw.to_owned())
    };

    let Some(resolved) = maybe_resolved else {
        return default;
    };

    let trimmed = resolved.trim();
    if trimmed.is_empty() {
        return default;
    }

    expand_path(trimmed)
}

fn expand_path(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }

    if let Some(suffix) = value.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(suffix);
        }
    }

    Path::new(value).to_path_buf()
}

fn positive_u64(
    value: Option<&Value>,
    path: &str,
    default: Option<u64>,
    errors: &mut Vec<ValidationError>,
) -> Option<u64> {
    parse_integer_field(value, path, default, errors, true)
}

fn positive_u32(
    value: Option<&Value>,
    path: &str,
    default: Option<u32>,
    errors: &mut Vec<ValidationError>,
) -> Option<u32> {
    parse_integer_field(value, path, default.map(u64::from), errors, true)
        .and_then(|value| u32::try_from(value).ok())
}

fn positive_usize(
    value: Option<&Value>,
    path: &str,
    default: Option<usize>,
    errors: &mut Vec<ValidationError>,
) -> Option<usize> {
    parse_integer_field(value, path, default.map(|value| value as u64), errors, true)
        .and_then(|value| usize::try_from(value).ok())
}

fn non_negative_u64(
    value: Option<&Value>,
    path: &str,
    default: Option<u64>,
    errors: &mut Vec<ValidationError>,
) -> Option<u64> {
    parse_integer_field(value, path, default, errors, false)
}

fn parse_integer_field(
    value: Option<&Value>,
    path: &str,
    default: Option<u64>,
    errors: &mut Vec<ValidationError>,
    require_positive: bool,
) -> Option<u64> {
    match value {
        None | Some(Value::Null) => default,
        Some(raw) => match parse_integer(raw) {
            Some(parsed) if require_positive && parsed > 0 => Some(parsed as u64),
            Some(parsed) if !require_positive && parsed >= 0 => Some(parsed as u64),
            Some(_) => {
                errors.push(ValidationError::new(
                    path,
                    if require_positive {
                        ConfigValueError::MustBePositive
                    } else {
                        ConfigValueError::MustBeNonNegative
                    },
                ));
                default
            }
            None => {
                errors.push(ValidationError::new(
                    path,
                    ConfigValueError::ExpectedInteger,
                ));
                default
            }
        },
    }
}

fn optional_u16(
    value: Option<&Value>,
    path: &str,
    errors: &mut Vec<ValidationError>,
) -> Option<u16> {
    match value {
        None | Some(Value::Null) => None,
        Some(raw) => match parse_integer(raw) {
            Some(parsed) if (0..=u16::MAX as i64).contains(&parsed) => Some(parsed as u16),
            Some(_) => {
                errors.push(ValidationError::new(
                    path,
                    ConfigValueError::MustBeNonNegative,
                ));
                None
            }
            None => {
                errors.push(ValidationError::new(
                    path,
                    ConfigValueError::ExpectedInteger,
                ));
                None
            }
        },
    }
}

fn parse_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(raw) => raw.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn env_reference_name(value: &str) -> Option<&str> {
    let candidate = value.strip_prefix('$')?;
    let mut chars = candidate.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }

    if chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        Some(candidate)
    } else {
        None
    }
}

fn normalize_secret_value(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim().to_owned();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn resolve_env_secret(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .and_then(|value| normalize_secret_value(Some(value)))
}

fn default_workspace_root() -> PathBuf {
    env::temp_dir().join(DEFAULT_WORKSPACE_DIR)
}
