mod cli;
mod daemon;
mod indexer;
mod search;
mod shared;
mod storage;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();

    let filter = match cli.verbose {
        0 => EnvFilter::new("warn"),
        1 => EnvFilter::new("info"),
        2 => EnvFilter::new("debug"),
        _ => EnvFilter::new("trace"),
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = cli::run(cli).await {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
