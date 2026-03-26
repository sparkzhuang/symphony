use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::Parser;
use symphony_rust::cli::Cli;
use symphony_rust::config::{validate_dispatch_preflight, ValidationError, WorkflowStore};
use symphony_rust::logging::{init_logging, LoggingConfig};
use symphony_rust::orchestrator::OrchestratorHandle;
use symphony_rust::server::{self, ServerHandle};

struct AppRuntime {
    orchestrator: OrchestratorHandle,
    server: Option<ServerHandle>,
}

impl AppRuntime {
    async fn start(
        workflow_path: PathBuf,
        store: WorkflowStore,
        server_port: Option<u16>,
        headless: bool,
    ) -> Result<Self> {
        let orchestrator = OrchestratorHandle::start(workflow_path.clone(), store);
        let server = match server_port {
            Some(port) => {
                let server = server::start(port).await?;
                tracing::info!(
                    server_url = %format!("http://{}", server.local_addr()),
                    "http_status_surface started status=ready"
                );
                Some(server)
            }
            None => None,
        };

        tracing::info!(
            workflow_path = %workflow_path.display(),
            headless,
            "runtime started status=ready"
        );

        Ok(Self {
            orchestrator,
            server,
        })
    }

    async fn wait_for_signal_or_failure(&mut self) -> Result<()> {
        if let Some(server) = self.server.as_mut() {
            tokio::select! {
                result = wait_for_shutdown_signal() => result,
                result = self.orchestrator.wait() => {
                    result?;
                    bail!("orchestrator exited unexpectedly");
                }
                result = server.wait() => {
                    result?;
                    bail!("http status surface exited unexpectedly");
                }
            }
        } else {
            tokio::select! {
                result = wait_for_shutdown_signal() => result,
                result = self.orchestrator.wait() => {
                    result?;
                    bail!("orchestrator exited unexpectedly");
                }
            }
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.orchestrator.request_shutdown();
        tracing::info!("poll_loop stopping status=stopping");
        let orchestrator_result = self.orchestrator.wait().await;

        let server_result = if let Some(server) = self.server.as_mut() {
            tracing::info!("http_status_surface stopping status=stopping");
            server.request_shutdown();
            server.wait().await
        } else {
            Ok(())
        };

        tracing::info!("agent_sessions stopping status=stopping");
        tracing::info!("logs flush status=completed");

        orchestrator_result?;
        server_result?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(cli, stdout_has_terminal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli, has_terminal: bool) -> Result<()> {
    cli.require_guardrails_acknowledgement()?;

    let workflow_path = cli.validate_workflow_path()?;
    let logs_root = cli.expanded_logs_root()?;
    init_logging(&LoggingConfig::from_logs_root(logs_root.as_deref()))?;

    let store = WorkflowStore::load(&workflow_path)
        .with_context(|| format!("failed to load workflow {}", workflow_path.display()))?;
    let mut config = store.current().config.clone();
    cli.apply_overrides(&mut config);

    let validation_errors = validate_dispatch_preflight(&config);
    if !validation_errors.is_empty() {
        bail!(format_validation_errors(&validation_errors));
    }

    let headless = cli.is_headless(has_terminal);
    tracing::info!(
        workflow_path = %workflow_path.display(),
        server_port = ?config.server.port,
        headless,
        "startup starting status=initializing"
    );

    if headless {
        tracing::info!("headless_mode enabled status=log_only");
    }

    let mut runtime = AppRuntime::start(workflow_path, store, config.server.port, headless).await?;
    runtime.wait_for_signal_or_failure().await?;
    runtime.shutdown().await?;
    Ok(())
}

fn stdout_has_terminal() -> bool {
    std::io::stdout().is_terminal() || std::io::stderr().is_terminal()
}

fn format_validation_errors(errors: &[ValidationError]) -> String {
    let details = errors
        .iter()
        .map(|error| format!("{}: {:?}", error.path, error.kind))
        .collect::<Vec<_>>()
        .join(", ");

    format!("Workflow validation failed: {details}")
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("failed to install SIGTERM handler")?;

        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("failed to listen for SIGINT")?;
            }
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for shutdown signal")?;
    }

    Ok(())
}
