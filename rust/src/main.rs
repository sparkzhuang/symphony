use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use symphony_rust::cli::Cli;
use symphony_rust::config::{validate_dispatch_preflight, ValidationError, WorkflowStore};
use symphony_rust::logging::{init_logging, LoggingConfig};
use symphony_rust::server::{self, ServerHandle};
use tokio::sync::watch;

struct AppRuntime {
    shutdown_tx: watch::Sender<bool>,
    poll_task: tokio::task::JoinHandle<()>,
    workflow_watcher: tokio::task::JoinHandle<()>,
    server: Option<ServerHandle>,
}

impl AppRuntime {
    async fn start(
        workflow_path: PathBuf,
        poll_interval_ms: u64,
        server_port: Option<u16>,
        headless: bool,
    ) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let poll_task = tokio::spawn(run_poll_loop(poll_interval_ms, shutdown_rx.clone()));
        let workflow_watcher = tokio::spawn(run_workflow_watcher(
            workflow_path.clone(),
            shutdown_rx.clone(),
        ));
        let server = match server_port {
            Some(port) => {
                let server = server::start(port, shutdown_rx.clone()).await?;
                tracing::info!(
                    server_url = %format!("http://{}", server.local_addr()),
                    "HTTP status surface ready"
                );
                Some(server)
            }
            None => None,
        };

        tracing::info!(
            workflow_path = %workflow_path.display(),
            headless,
            "Symphony runtime started"
        );

        Ok(Self {
            shutdown_tx,
            poll_task,
            workflow_watcher,
            server,
        })
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        tracing::info!("Stopping poll loop");
        let _ = self.poll_task.await;
        tracing::info!("Stopping workflow watcher");
        let _ = self.workflow_watcher.await;
        if let Some(server) = self.server {
            tracing::info!("Stopping HTTP status surface");
            let _ = server.wait().await;
        }
        tracing::info!("Terminating agents");
        tracing::info!("Flushing logs");
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
        "Starting Symphony"
    );

    if headless {
        tracing::info!("Headless mode active; logs remain the only output surface");
    }

    let runtime = AppRuntime::start(
        workflow_path,
        config.polling.interval_ms,
        config.server.port,
        headless,
    )
    .await?;
    wait_for_shutdown_signal().await?;
    runtime.shutdown().await;
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

async fn run_poll_loop(poll_interval_ms: u64, mut shutdown: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(Duration::from_millis(poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                tracing::debug!(poll_interval_ms, "Poll loop tick");
            }
            result = shutdown.changed() => {
                if result.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

async fn run_workflow_watcher(workflow_path: PathBuf, mut shutdown: watch::Receiver<bool>) {
    let mut store = match WorkflowStore::load(&workflow_path) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(
                workflow_path = %workflow_path.display(),
                error = %error,
                "Failed to initialize workflow watcher"
            );
            return;
        }
    };

    let mut interval = tokio::time::interval(symphony_rust::config::WORKFLOW_POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(error) = store.reload_if_changed_async().await {
                    tracing::warn!(
                        workflow_path = %workflow_path.display(),
                        error = %error,
                        "Workflow reload failed; keeping last good snapshot"
                    );
                }
            }
            result = shutdown.changed() => {
                if result.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}
