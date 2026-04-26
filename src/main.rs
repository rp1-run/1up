mod cli;
mod daemon;
mod indexer;
mod mcp;
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

    let show_notification = should_show_notification(&cli.command);

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
/// - Maintenance commands explicitly selecting JSON output (AC-02c). The
///   format moved off the global `Cli` onto each maintenance Args struct, so
///   we read it from the selected command's args rather than a global field.
/// - The internal Worker command (AC-02d)
/// - The Update command (it handles its own update output)
fn should_show_notification(command: &cli::Command) -> bool {
    if let Some(format) = maintenance_format(command) {
        if format == OutputFormat::Json {
            return false;
        }
    }
    !matches!(
        command,
        cli::Command::Mcp(_) | cli::Command::Worker | cli::Command::Update(_)
    )
}

/// Extract the explicit maintenance `--format`/`-f` selection, if any. Returns
/// `None` for core commands (which have no format flag) and for maintenance
/// commands where the user did not pass the flag.
fn maintenance_format(command: &cli::Command) -> Option<OutputFormat> {
    match command {
        cli::Command::Start(args) => args.format,
        cli::Command::Stop(args) => args.format,
        cli::Command::Status(args) => args.format,
        cli::Command::Init(args) => args.format,
        cli::Command::Index(args) => args.format,
        cli::Command::Reindex(args) => args.format,
        cli::Command::Update(args) => args.format,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_suppresses_passive_update_notification() {
        let cli = cli::Cli::parse_from(["1up", "mcp"]);

        assert!(!should_show_notification(&cli.command));
    }
}
