use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tokio::sync::watch;

use crate::agent::{AgentRunner, AgentRunnerError, AppServerEvent, AppServerEventKind};
use crate::config::{TrackerConfig, WorkflowSnapshot, WorkflowStore};
use crate::tracker::linear::LinearTracker;
use crate::tracker::{AssigneeMode, LinearTrackerConfig, Tracker, TrackerAdapter};
use crate::types::Issue;

pub struct OrchestratorHandle {
    shutdown_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl OrchestratorHandle {
    pub fn start(workflow_path: PathBuf, store: WorkflowStore) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_loop(workflow_path, store, shutdown_rx));

        Self { shutdown_tx, task }
    }

    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub async fn wait(&mut self) -> Result<()> {
        (&mut self.task)
            .await
            .context("orchestrator task join failed")?
    }
}

async fn run_loop(
    workflow_path: PathBuf,
    mut store: WorkflowStore,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        let snapshot = store.current().clone();
        poll_once(&snapshot, &mut shutdown).await?;

        let sleep = tokio::time::sleep(Duration::from_millis(snapshot.config.polling.interval_ms));
        tokio::pin!(sleep);

        tokio::select! {
            _ = &mut sleep => {}
            result = shutdown.changed() => {
                if result.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
        }

        if let Err(error) = store.reload_if_changed_async().await {
            tracing::warn!(
                workflow_path = %workflow_path.display(),
                error = %error,
                "workflow_reload failed status=retaining_last_good_snapshot"
            );
        }
    }

    Ok(())
}

async fn poll_once(
    snapshot: &WorkflowSnapshot,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    if *shutdown.borrow() {
        return Ok(());
    }

    let tracker: Arc<dyn Tracker> = Arc::new(build_tracker(&snapshot.config.tracker)?);
    let issues = match tracker.fetch_candidate_issues().await {
        Ok(issues) => issues,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "tracker_poll failed status=retrying_next_cycle"
            );
            return Ok(());
        }
    };

    tracing::info!(
        issue_count = issues.len(),
        "tracker_poll completed status=ok"
    );

    for issue in issues {
        if *shutdown.borrow() {
            break;
        }

        if let Err(error) = run_issue(snapshot, Arc::clone(&tracker), issue, shutdown.clone()).await
        {
            match error.downcast_ref::<AgentRunnerError>() {
                Some(AgentRunnerError::ShutdownRequested) => break,
                _ => tracing::error!(error = %error, "issue_run failed status=failed"),
            }
        }
    }

    Ok(())
}

async fn run_issue(
    snapshot: &WorkflowSnapshot,
    tracker: Arc<dyn Tracker>,
    issue: Issue,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let issue_id = issue.id.as_str().to_owned();
    let issue_identifier = issue.identifier.as_str().to_owned();
    let issue_title = issue.title.clone();
    let issue_span = tracing::info_span!(
        "issue_run",
        issue_id = issue_id.as_str(),
        issue_identifier = issue_identifier.as_str()
    );
    let _issue_guard = issue_span.enter();

    tracing::info!(title = %issue_title, "issue_run starting status=dispatching");

    let runner = AgentRunner::new(
        snapshot.workflow.clone(),
        snapshot.config.clone(),
        Arc::clone(&tracker),
    )
    .with_context(|| format!("failed to initialize agent runner for {issue_identifier}"))?;

    let sink_issue_id = issue_id.clone();
    let sink_issue_identifier = issue_identifier.clone();
    let sink = Arc::new(move |event: AppServerEvent| {
        log_agent_event(&sink_issue_id, &sink_issue_identifier, &event);
    });

    match runner.run(issue, Some(sink), Some(shutdown)).await {
        Ok(result) => {
            tracing::info!(
                workspace_path = %result.workspace.path.display(),
                turn_count = result.turn_count,
                "issue_run completed status=succeeded"
            );
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("issue run failed for {issue_identifier}"))
        }
    }
}

fn log_agent_event(issue_id: &str, issue_identifier: &str, event: &AppServerEvent) {
    let session_id = event
        .payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let session_span = tracing::info_span!("agent_event", issue_id, issue_identifier, session_id);
    let _session_guard = session_span.enter();

    match event.kind {
        AppServerEventKind::SessionStarted => {
            tracing::info!(payload = %event.payload, "agent_session started status=completed");
        }
        AppServerEventKind::TurnCompleted => {
            tracing::info!(payload = %event.payload, "agent_turn completed status=completed");
        }
        AppServerEventKind::TurnFailed | AppServerEventKind::ToolCallFailed => {
            tracing::warn!(payload = %event.payload, "agent_turn completed status=failed");
        }
        AppServerEventKind::TurnCancelled => {
            tracing::info!(payload = %event.payload, "agent_turn completed status=cancelled");
        }
        AppServerEventKind::Malformed | AppServerEventKind::StartupFailed => {
            tracing::warn!(payload = %event.payload, "agent_event completed status=failed");
        }
        _ => {
            tracing::debug!(payload = %event.payload, kind = ?event.kind, "agent_event observed");
        }
    }
}

fn build_tracker(config: &TrackerConfig) -> Result<TrackerAdapter> {
    match config.kind.as_deref() {
        Some("linear") => Ok(TrackerAdapter::Linear(LinearTracker::new(
            LinearTrackerConfig {
                endpoint: config.endpoint.clone(),
                api_key: config.api_key.clone(),
                project_slug: config.project_slug.clone(),
                active_states: config.active_states.clone(),
                assignee: assignee_mode(config.assignee.as_deref()),
                page_size: crate::tracker::DEFAULT_LINEAR_PAGE_SIZE,
                timeout: crate::tracker::DEFAULT_LINEAR_TIMEOUT,
            },
        )?)),
        Some(other) => Err(anyhow!("unsupported tracker.kind: {other}")),
        None => Err(anyhow!("missing tracker.kind")),
    }
}

fn assignee_mode(value: Option<&str>) -> AssigneeMode {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => AssigneeMode::Any,
        Some("me") => AssigneeMode::Me,
        Some(value) => AssigneeMode::Id(value.to_owned()),
    }
}
