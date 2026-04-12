mod cli;
mod daemon;
mod indexer;
mod search;
mod shared;
mod storage;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::shared::types::OutputFormat;

#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();

    let filter = match cli.verbose {
        0 => EnvFilter::new("error"),
        1 => EnvFilter::new("warn"),
        2 => EnvFilter::new("info,1up=debug"),
        _ => EnvFilter::new("trace"),
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let format = cli.resolved_format();
    let show_notification = should_show_notification(format, &cli.command);

    let refresh_handle = if show_notification {
        Some(tokio::spawn(shared::update::refresh_cache_if_stale()))
    } else {
        None
    };

    if let Err(e) = cli::run(cli).await {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }

    if show_notification {
        if let Some(handle) = refresh_handle {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        }
        if let Some(notice) = shared::update::format_update_notification() {
            eprintln!("{notice}");
        }
    }
}

/// Returns `true` when a passive update notification should be shown after the
/// command completes.
///
/// Notifications are suppressed for:
/// - JSON output mode (AC-02c)
/// - The internal Worker command (AC-02d)
/// - The Update command (it handles its own update output)
fn should_show_notification(format: OutputFormat, command: &cli::Command) -> bool {
    if format == OutputFormat::Json {
        return false;
    }
    !matches!(command, cli::Command::Worker | cli::Command::Update(_))
}
