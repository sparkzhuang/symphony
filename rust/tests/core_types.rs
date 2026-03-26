use std::collections::{HashMap, HashSet};

use chrono::Utc;
use symphony_rust::types::{
    BlockerRef, CodexTotals, Issue, IssueId, IssueIdentifier, LiveSession, OrchestratorState,
    RetryEntry, RunAttempt, RunStatus, RunningEntry, WorkflowDefinition, Workspace,
};

#[test]
fn defines_all_spec_run_status_variants() {
    let statuses = [
        RunStatus::PreparingWorkspace,
        RunStatus::BuildingPrompt,
        RunStatus::LaunchingAgentProcess,
        RunStatus::InitializingSession,
        RunStatus::StreamingTurn,
        RunStatus::Finishing,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::TimedOut,
        RunStatus::Stalled,
        RunStatus::CanceledByReconciliation,
    ];

    assert_eq!(statuses.len(), 11);
}

#[test]
fn sanitizes_workspace_keys_from_issue_identifiers() {
    let workspace = Workspace::for_issue_identifier("SPA-8 / core types", true);

    assert_eq!(workspace.workspace_key.as_str(), "SPA-8___core_types");
}

#[test]
fn core_types_round_trip_through_serde() {
    let issue = Issue {
        id: IssueId::new("92d669f5"),
        identifier: IssueIdentifier::new("SPA-8"),
        title: "Project Scaffold + Core Types".to_owned(),
        description: Some("Create the Rust project scaffold".to_owned()),
        priority: Some(1),
        state: "In Progress".to_owned(),
        branch_name: Some("feature/SPA-8".to_owned()),
        url: Some("https://linear.app/sparkzhuang/issue/SPA-8".to_owned()),
        labels: vec!["module-1".to_owned(), "rust".to_owned()],
        blocked_by: vec![BlockerRef {
            id: Some(IssueId::new("dep-1")),
            identifier: Some(IssueIdentifier::new("SPA-7")),
            state: Some("Done".to_owned()),
        }],
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
    };

    let running_entry = RunningEntry {
        issue: issue.clone(),
        run_attempt: RunAttempt {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            attempt: None,
            workspace_path: std::path::PathBuf::from("/tmp/SPA-8"),
            started_at: Utc::now(),
            status: RunStatus::StreamingTurn,
            error: None,
        },
        live_session: Some(LiveSession::new("thread-1", "turn-1")),
        worker_host: Some("localhost".to_owned()),
    };

    let state = OrchestratorState {
        poll_interval_ms: 30_000,
        max_concurrent_agents: 4,
        running: HashMap::from([(issue.id.clone(), running_entry)]),
        claimed: HashSet::from([issue.id.clone()]),
        retry_attempts: HashMap::from([(
            issue.id.clone(),
            RetryEntry {
                issue_id: issue.id.clone(),
                identifier: issue.identifier.clone(),
                attempt: 1,
                due_at_ms: 123_456,
                timer_handle: Some("timer-ref".to_owned()),
                error: Some("transient failure".to_owned()),
            },
        )]),
        completed: HashSet::new(),
        codex_totals: CodexTotals::default(),
        codex_rate_limits: None,
    };

    let workflow = WorkflowDefinition::new(
        serde_json::json!({"polling": {"interval_ms": 30000}}),
        "# Prompt",
    );

    let encoded_state = serde_json::to_string(&state).expect("orchestrator state should serialize");
    let decoded_state: OrchestratorState =
        serde_json::from_str(&encoded_state).expect("orchestrator state should deserialize");
    let encoded_workflow = serde_json::to_string(&workflow).expect("workflow should serialize");
    let decoded_workflow: WorkflowDefinition =
        serde_json::from_str(&encoded_workflow).expect("workflow should deserialize");

    assert_eq!(decoded_state.poll_interval_ms, 30_000);
    assert_eq!(decoded_state.max_concurrent_agents, 4);
    assert_eq!(decoded_state.running.len(), 1);
    assert_eq!(decoded_workflow.prompt_template, "# Prompt");
}
