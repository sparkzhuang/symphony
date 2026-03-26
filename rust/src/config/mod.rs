//! Configuration schema, workflow loading, and hot-reload support.

pub mod schema;
pub mod workflow;

pub use schema::{
    validate_dispatch_preflight, AgentConfig, CodexConfig, ConfigError, ConfigValueError,
    HooksConfig, PollingConfig, ServerConfig, TrackerConfig, ValidationError, WorkflowConfig,
    WorkspaceConfig,
};
pub use workflow::{
    parse_workflow, WorkflowError, WorkflowSnapshot, WorkflowStamp, WorkflowStore,
    WORKFLOW_POLL_INTERVAL,
};
