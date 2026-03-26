use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "symphony-rust",
    about = "Rust implementation of the Symphony service"
)]
struct Cli {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    tracing::info!("Symphony Rust stub starting");
    Ok(())
}
