use clap::Parser;
use std::env;

use symphony_rust::server;

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

    match server::bind_from_workflow(&workflow_path, cli.port).await? {
        Some(server) => {
            let local_addr = server.local_addr();
            tracing::info!(%local_addr, "HTTP observability server listening");
            tokio::signal::ctrl_c().await?;
            server.shutdown().await;
        }
        None => {
            tracing::info!("HTTP observability server disabled");
        }
    }

    Ok(())
}
