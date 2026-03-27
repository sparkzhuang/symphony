use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context};
use clap::Parser;
use symphony_rust::cli::Cli;
use symphony_rust::config::{
    validate_dispatch_preflight, ValidationError, WorkflowConfig, WorkflowStore,
};
use symphony_rust::dashboard::terminal::{
    StdoutDashboardSink, TerminalDashboard, TerminalDashboardConfig,
};
use symphony_rust::dashboard::Dashboard;
use symphony_rust::logging::{init_logging, LoggingConfig};
use symphony_rust::orchestrator::{Orchestrator, OrchestratorHandle, OrchestratorRefreshHandle};
use symphony_rust::server::{HttpServer, ServerOptions, Snapshot, SnapshotState};
use symphony_rust::tracker::linear::LinearTracker;
use symphony_rust::tracker::{AssigneeMode, LinearTrackerConfig, Tracker, TrackerAdapter};
use symphony_rust::types::{OrchestratorState, WorkflowDefinition};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Duration, MissedTickBehavior};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let logging = match init_logging(&LoggingConfig {
        logs_root: cli.logs_root.clone(),
    }) {
        Ok(logging) => logging,
        Err(error) => {
            eprintln!("failed to initialize logging: {error:#}");
            return ExitCode::from(1);
        }
    };

    let exit_code = match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(error = %error, "symphony host failed");
            ExitCode::from(1)
        }
    };

    let _ = logging;
    flush_stdio();
    exit_code
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let stdout_is_terminal = io::stdout().is_terminal();
    let mut runtime = start_runtime(cli, stdout_is_terminal).await?;
    info!(
        workflow_path = %runtime.workflow_path.display(),
        port = runtime.port.unwrap_or_default(),
        headless = runtime.headless,
        action = "started",
        "symphony host started"
    );

    tokio::select! {
        signal = wait_for_shutdown_signal() => {
            signal?;
            info!(action = "shutdown", "shutdown signal received");
        }
        result = runtime.wait_for_exit() => {
            result?;
            bail!("orchestrator host exited unexpectedly");
        }
    }

    runtime.shutdown().await
}

fn require_guardrails_acknowledgement(cli: &Cli) -> anyhow::Result<()> {
    if cli.guardrails_acknowledged {
        return Ok(());
    }

    bail!(
        "This Symphony implementation runs without the usual guardrails. Re-run with \
         --i-understand-that-this-will-be-running-without-the-usual-guardrails."
    )
}

fn apply_cli_overrides(config: &mut WorkflowConfig, cli: &Cli) {
    if let Some(port) = cli.port {
        config.server.port = Some(port);
    }
}

fn validate_startup_config(config: &WorkflowConfig) -> anyhow::Result<()> {
    let errors = validate_dispatch_preflight(config);
    if errors.is_empty() {
        return Ok(());
    }

    bail!(format_validation_errors(&errors))
}

fn format_validation_errors(errors: &[ValidationError]) -> String {
    let joined = errors
        .iter()
        .map(|error| format!("{}: {:?}", error.path, error.kind))
        .collect::<Vec<_>>()
        .join(", ");
    format!("workflow startup validation failed: {joined}")
}

fn build_tracker(config: &WorkflowConfig) -> anyhow::Result<Arc<dyn Tracker>> {
    match config.tracker.kind.as_deref() {
        Some("linear") => {
            let tracker = LinearTracker::new(build_linear_tracker_config(config))
                .context("failed to build linear tracker")?;
            Ok(Arc::new(TrackerAdapter::Linear(tracker)))
        }
        Some(other) => bail!("unsupported tracker kind: {other}"),
        None => bail!("tracker.kind is required"),
    }
}

fn build_linear_tracker_config(config: &WorkflowConfig) -> LinearTrackerConfig {
    LinearTrackerConfig {
        endpoint: config.tracker.endpoint.clone(),
        api_key: config.tracker.api_key.clone(),
        project_slug: config.tracker.project_slug.clone(),
        active_states: config.tracker.active_states.clone(),
        assignee: match config.tracker.assignee.as_deref().map(str::trim) {
            Some("me") => AssigneeMode::Me,
            Some(value) if !value.is_empty() => AssigneeMode::Id(value.to_owned()),
            _ => AssigneeMode::Any,
        },
        ..LinearTrackerConfig::default()
    }
}

fn is_headless(port: Option<u16>, stdout_is_terminal: bool) -> bool {
    port.is_none() && !stdout_is_terminal
}

struct AppRuntime {
    workflow_path: PathBuf,
    port: Option<u16>,
    headless: bool,
    dashboard_enabled: bool,
    orchestrator: OrchestratorHandle,
    server: Option<HttpServer>,
    server_snapshot_task: Option<JoinHandle<()>>,
    refresh_task: Option<JoinHandle<()>>,
    dashboard_task: Option<JoinHandle<()>>,
}

impl AppRuntime {
    #[cfg(test)]
    fn server_addr(&self) -> Option<std::net::SocketAddr> {
        self.server.as_ref().map(HttpServer::local_addr)
    }

    #[cfg(test)]
    fn dashboard_enabled(&self) -> bool {
        self.dashboard_enabled
    }

    async fn wait_for_exit(&mut self) -> anyhow::Result<()> {
        self.orchestrator.wait_for_exit().await
    }

    async fn shutdown(mut self) -> anyhow::Result<()> {
        info!(action = "shutdown", "stopping orchestrator");
        self.orchestrator
            .shutdown()
            .await
            .context("failed to shut down orchestrator")?;

        if let Some(server) = self.server.take() {
            server.shutdown().await;
        }

        join_optional_task(self.server_snapshot_task.take()).await;
        join_optional_task(self.refresh_task.take()).await;
        join_optional_task(self.dashboard_task.take()).await;
        flush_stdio();
        Ok(())
    }
}

async fn start_runtime(cli: Cli, stdout_is_terminal: bool) -> anyhow::Result<AppRuntime> {
    require_guardrails_acknowledgement(&cli)?;

    let (workflow_path, workflow, config) = load_runtime_inputs(&cli)?;
    let tracker = build_tracker(&config)?;
    let orchestrator =
        Orchestrator::new_with_workflow(workflow, config.clone(), Arc::clone(&tracker))
            .context("failed to build orchestrator")?
            .spawn()
            .context("failed to start orchestrator")?;

    let headless = is_headless(config.server.port, stdout_is_terminal);
    let dashboard_enabled = !headless && stdout_is_terminal;

    let mut runtime = AppRuntime {
        workflow_path,
        port: config.server.port,
        headless,
        dashboard_enabled,
        orchestrator,
        server: None,
        server_snapshot_task: None,
        refresh_task: None,
        dashboard_task: None,
    };

    if let Err(error) = initialize_runtime_surfaces(&mut runtime, &config).await {
        runtime.shutdown().await?;
        return Err(error);
    }

    Ok(runtime)
}

fn load_runtime_inputs(cli: &Cli) -> anyhow::Result<(PathBuf, WorkflowDefinition, WorkflowConfig)> {
    let workflow_path = cli.workflow_path();
    let store = WorkflowStore::load(&workflow_path).with_context(|| {
        format!(
            "failed to load workflow definition from {}",
            workflow_path.display()
        )
    })?;
    let mut config = store.current().config.clone();
    apply_cli_overrides(&mut config, cli);
    validate_startup_config(&config)?;

    Ok((workflow_path, store.current().workflow.clone(), config))
}

async fn initialize_runtime_surfaces(
    runtime: &mut AppRuntime,
    config: &WorkflowConfig,
) -> anyhow::Result<()> {
    if config.server.port.is_some() {
        let (server, snapshot_task, refresh_task) =
            start_http_server(config, &runtime.orchestrator)
                .await
                .context("failed to start HTTP server")?;
        runtime.server = Some(server);
        runtime.server_snapshot_task = Some(snapshot_task);
        runtime.refresh_task = Some(refresh_task);
    }

    if runtime.dashboard_enabled {
        runtime.dashboard_task = Some(tokio::spawn(run_dashboard(
            runtime.orchestrator.subscribe_state(),
        )));
    }

    Ok(())
}

async fn start_http_server(
    config: &WorkflowConfig,
    orchestrator: &OrchestratorHandle,
) -> anyhow::Result<(HttpServer, JoinHandle<()>, JoinHandle<()>)> {
    let initial_snapshot = SnapshotState::Ready(Box::new(Snapshot::new(
        orchestrator.subscribe_state().borrow().clone(),
        chrono::Utc::now(),
        0,
    )));
    let (snapshot_tx, snapshot_rx) = watch::channel(initial_snapshot);
    let (refresh_tx, refresh_rx) = mpsc::channel(8);

    let server = HttpServer::bind(
        ServerOptions::new(snapshot_rx, refresh_tx)
            .with_config_port(config.server.port)
            .with_workspace_root(config.workspace.root.clone()),
    )
    .await?;

    let snapshot_task = tokio::spawn(forward_server_snapshots(
        orchestrator.subscribe_state(),
        snapshot_tx,
    ));
    let refresh_task = tokio::spawn(forward_refresh_requests(
        refresh_rx,
        orchestrator.refresh_handle(),
    ));

    Ok((server, snapshot_task, refresh_task))
}

async fn forward_server_snapshots(
    mut state_rx: watch::Receiver<OrchestratorState>,
    snapshot_tx: watch::Sender<SnapshotState>,
) {
    let started_at = Instant::now();

    loop {
        let snapshot = SnapshotState::Ready(Box::new(Snapshot::new(
            state_rx.borrow().clone(),
            chrono::Utc::now(),
            elapsed_millis(started_at),
        )));
        let _ = snapshot_tx.send(snapshot);

        if state_rx.changed().await.is_err() {
            let _ = snapshot_tx.send(SnapshotState::Unavailable);
            break;
        }
    }
}

async fn forward_refresh_requests(
    mut refresh_rx: mpsc::Receiver<()>,
    refresh_handle: OrchestratorRefreshHandle,
) {
    while refresh_rx.recv().await.is_some() {
        if let Err(error) = refresh_handle.request_refresh() {
            warn!(error = %error, "failed to enqueue refresh request");
            break;
        }
    }
}

async fn run_dashboard(state_rx: watch::Receiver<OrchestratorState>) {
    let mut dashboard =
        TerminalDashboard::new(TerminalDashboardConfig::default(), StdoutDashboardSink);
    let started_at = Instant::now();
    let mut state_rx = state_rx;
    let _ = render_dashboard_snapshot(&mut dashboard, &state_rx.borrow().clone(), started_at);

    let mut ticker = tokio::time::interval(Duration::from_millis(
        dashboard.config().refresh_interval_ms,
    ));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            result = state_rx.changed() => {
                if result.is_err() {
                    break;
                }

                let snapshot = state_rx.borrow().clone();
                let _ = render_dashboard_snapshot(&mut dashboard, &snapshot, started_at);
            }
            _ = ticker.tick() => {
                let now_ms = elapsed_millis(started_at);
                if let Err(error) = dashboard.flush_pending(now_ms) {
                    warn!(error = %error, "failed to flush dashboard");
                }
            }
        }
    }
}

fn render_dashboard_snapshot(
    dashboard: &mut TerminalDashboard<StdoutDashboardSink>,
    snapshot: &OrchestratorState,
    started_at: Instant,
) -> io::Result<()> {
    let now_ms = elapsed_millis(started_at);
    dashboard
        .render_snapshot(snapshot, chrono::Utc::now(), now_ms)
        .map(|_| ())
}

fn elapsed_millis(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

async fn join_optional_task(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        match task.await {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) => warn!(error = %error, "background task exited unexpectedly"),
        }
    }
}

async fn wait_for_shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("failed to install SIGTERM handler")?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Ok(()),
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed to install CTRL-C handler")
    }
}

fn flush_stdio() {
    let _ = io::stdout().lock().flush();
    let _ = io::stderr().lock().flush();
}

#[cfg(test)]
mod tests {
    use super::{
        apply_cli_overrides, build_linear_tracker_config, format_validation_errors, is_headless,
        require_guardrails_acknowledgement, start_runtime,
    };
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use symphony_rust::cli::Cli;
    use symphony_rust::config::{ConfigValueError, ValidationError, WorkflowConfig};
    use symphony_rust::tracker::AssigneeMode;

    #[test]
    fn cli_port_override_wins_over_workflow_port() {
        let mut config = minimal_workflow_config();
        config.server.port = Some(3000);
        let cli = Cli {
            workflow_path: "./WORKFLOW.md".into(),
            port: Some(4000),
            logs_root: None,
            guardrails_acknowledged: true,
        };

        apply_cli_overrides(&mut config, &cli);

        assert_eq!(config.server.port, Some(4000));
    }

    #[test]
    fn headless_mode_requires_no_port_and_no_terminal() {
        assert!(is_headless(None, false));
        assert!(!is_headless(Some(3000), false));
        assert!(!is_headless(None, true));
    }

    #[test]
    fn guardrails_acknowledgement_is_required() {
        let cli = Cli {
            workflow_path: "./WORKFLOW.md".into(),
            port: None,
            logs_root: None,
            guardrails_acknowledged: false,
        };

        let error = require_guardrails_acknowledgement(&cli)
            .expect_err("startup should require acknowledgement");
        assert!(error.to_string().contains("without the usual guardrails"));
    }

    #[test]
    fn tracker_assignee_me_maps_to_me_mode() {
        let mut config = minimal_workflow_config();
        config.tracker.kind = Some("linear".to_owned());
        config.tracker.assignee = Some("me".to_owned());

        let tracker = build_linear_tracker_config(&config);

        assert!(matches!(tracker.assignee, AssigneeMode::Me));
    }

    #[test]
    fn validation_errors_are_joined_into_one_message() {
        let message = format_validation_errors(&[
            ValidationError::new("tracker.kind", ConfigValueError::Required),
            ValidationError::new("tracker.api_key", ConfigValueError::Required),
        ]);

        assert!(message.contains("tracker.kind"));
        assert!(message.contains("tracker.api_key"));
    }

    #[tokio::test]
    async fn startup_path_starts_http_server_when_cli_port_is_set() {
        let temp_dir = unique_temp_dir("main-startup-server");
        fs::create_dir_all(&temp_dir).expect("temp dir should exist");
        let workflow_path = temp_dir.join("WORKFLOW.md");
        write_workflow(&workflow_path);

        let cli = Cli {
            workflow_path: workflow_path.clone(),
            port: Some(0),
            logs_root: None,
            guardrails_acknowledged: true,
        };

        let runtime = start_runtime(cli, false)
            .await
            .expect("startup should succeed");

        let server = runtime.server_addr().expect("server should be started");
        let response = reqwest::get(format!("http://{server}/healthz"))
            .await
            .expect("health check should succeed");

        assert_eq!(response.status(), reqwest::StatusCode::OK);

        runtime.shutdown().await.expect("shutdown should succeed");
    }

    #[tokio::test]
    async fn startup_path_enables_terminal_surface_when_not_headless() {
        let temp_dir = unique_temp_dir("main-startup-dashboard");
        fs::create_dir_all(&temp_dir).expect("temp dir should exist");
        let workflow_path = temp_dir.join("WORKFLOW.md");
        write_workflow(&workflow_path);

        let cli = Cli {
            workflow_path,
            port: None,
            logs_root: None,
            guardrails_acknowledged: true,
        };

        let runtime = start_runtime(cli, true)
            .await
            .expect("startup should succeed");

        assert!(runtime.dashboard_enabled());
        assert!(runtime.server_addr().is_none());

        runtime.shutdown().await.expect("shutdown should succeed");
    }

    fn minimal_workflow_config() -> WorkflowConfig {
        WorkflowConfig::from_value(json!({})).expect("default config should parse")
    }

    fn write_workflow(path: &Path) {
        fs::write(
            path,
            r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
---
Prompt
"#,
        )
        .expect("workflow file should be written");
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{unique}"))
    }
}
