use anyhow::Context;
use clap::Parser;
use std::env;

use symphony_rust::server::{self, SnapshotState};

#[derive(Debug, Parser)]
#[command(
    name = "symphony-rust",
    about = "Rust implementation of the Symphony service"
)]
struct Cli {
    /// HTTP server port override for the optional observability API.
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let workflow_path = env::current_dir()?.join("WORKFLOW.md");
    let (mut runtime, snapshot_rx, refresh_tx) = server::runtime_channels(1);

    match server::bind_from_workflow(&workflow_path, cli.port, snapshot_rx, refresh_tx).await? {
        Some(server) => {
            let local_addr = server.local_addr();
            tracing::info!(%local_addr, "HTTP observability server listening");
            runtime
                .snapshot_sender()
                .send(SnapshotState::Unavailable)
                .context("failed to publish initial HTTP snapshot state")?;

            let refresh_task = tokio::spawn(async move {
                while runtime.refresh_receiver().recv().await.is_some() {
                    tracing::warn!(
                        "HTTP refresh requested before orchestrator integration is available"
                    );
                }
            });

            tokio::signal::ctrl_c().await?;
            server.shutdown().await;
            refresh_task.abort();
            let _ = refresh_task.await;
        }
        None => {
            tracing::info!("HTTP observability server disabled");
        }
    }

    Ok(())
}
