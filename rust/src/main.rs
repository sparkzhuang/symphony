use clap::Parser;

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

    if let Some(port) = cli.port {
        tracing::info!(port, "HTTP server CLI override requested");
    }

    tracing::info!("Symphony Rust stub starting");
    Ok(())
}
