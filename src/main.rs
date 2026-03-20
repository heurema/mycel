use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod cli;
mod config;
mod crypto;
mod envelope;
mod error;
mod nostr;
mod store;

use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("MYCEL_LOG"))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    cli.run().await
}
