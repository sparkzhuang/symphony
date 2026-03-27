use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::Parser;
use symphony_rust::cli::Cli;
use symphony_rust::config::{
    validate_dispatch_preflight, ValidationError, WorkflowConfig, WorkflowStore,
};
use symphony_rust::logging::{init_logging, LoggingConfig};
use symphony_rust::orchestrator::{Orchestrator, OrchestratorHandle};
use symphony_rust::tracker::linear::LinearTracker;
use symphony_rust::tracker::{AssigneeMode, LinearTrackerConfig, Tracker, TrackerAdapter};
use tracing::{error, info};

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
    require_guardrails_acknowledgement(&cli)?;

    let workflow_path = cli.workflow_path();
    let store = WorkflowStore::load(&workflow_path).with_context(|| {
        format!(
            "failed to load workflow definition from {}",
            workflow_path.display()
        )
    })?;
    let mut config = store.current().config.clone();
    apply_cli_overrides(&mut config, &cli);
    validate_startup_config(&config)?;

    let tracker = build_tracker(&config)?;
    let mut orchestrator = Orchestrator::new_with_workflow(
        store.current().workflow.clone(),
        config.clone(),
        Arc::clone(&tracker),
    )
    .context("failed to build orchestrator")?
    .spawn()
    .context("failed to start orchestrator")?;

    let headless = is_headless(config.server.port, io::stdout().is_terminal());
    info!(
        workflow_path = %workflow_path.display(),
        port = config.server.port.unwrap_or_default(),
        headless,
        action = "started",
        "symphony host started"
    );

    tokio::select! {
        signal = wait_for_shutdown_signal() => {
            signal?;
            info!(action = "shutdown", "shutdown signal received");
        }
        result = orchestrator.wait_for_exit() => {
            result?;
            bail!("orchestrator host exited unexpectedly");
        }
    }

    shutdown(orchestrator).await
}

async fn shutdown(orchestrator: OrchestratorHandle) -> anyhow::Result<()> {
    info!(action = "shutdown", "stopping orchestrator");
    orchestrator
        .shutdown()
        .await
        .context("failed to shut down orchestrator")?;
    flush_stdio();
    Ok(())
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
        require_guardrails_acknowledgement,
    };
    use serde_json::json;
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

    fn minimal_workflow_config() -> WorkflowConfig {
        WorkflowConfig::from_value(json!({})).expect("default config should parse")
    }
}
