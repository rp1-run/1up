pub mod context;
pub mod index;
pub mod init;
pub mod output;
pub mod reindex;
pub mod search;
pub mod start;
pub mod status;
pub mod stop;
pub mod symbol;

use clap::{Parser, Subcommand};

use crate::shared::types::OutputFormat;

#[derive(Parser)]
#[command(
    name = "1up",
    about = "Unified search substrate for source repositories",
    version,
    propagate_version = true
)]
pub struct Cli {
    /// Output format: json (default), human, plain
    #[arg(long, short, global = true, default_value = "json")]
    pub format: OutputFormat,

    /// Increase logging verbosity (-v for debug, -vv for trace)
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize a project for 1up indexing
    Init(init::InitArgs),

    /// Index and start the background daemon
    Start(start::StartArgs),

    /// Stop the background daemon
    Stop(stop::StopArgs),

    /// Show daemon and index status
    Status(status::StatusArgs),

    /// Look up symbol definitions and references
    Symbol(symbol::SymbolArgs),

    /// Hybrid semantic + full-text search
    Search(search::SearchArgs),

    /// Retrieve code context around a file location
    Context(context::ContextArgs),

    /// Index a repository
    Index(index::IndexArgs),

    /// Force re-index of all files
    Reindex(reindex::ReindexArgs),

    /// Internal: daemon worker process (not for direct use)
    #[command(name = "__worker", hide = true)]
    Worker,
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Init(args) => init::exec(args, cli.format).await,
        Command::Start(args) => start::exec(args, cli.format).await,
        Command::Stop(args) => stop::exec(args, cli.format).await,
        Command::Status(args) => status::exec(args, cli.format).await,
        Command::Symbol(args) => symbol::exec(args, cli.format).await,
        Command::Search(args) => search::exec(args, cli.format).await,
        Command::Context(args) => context::exec(args, cli.format).await,
        Command::Index(args) => index::exec(args, cli.format).await,
        Command::Reindex(args) => reindex::exec(args, cli.format).await,
        Command::Worker => crate::daemon::worker::run().await.map_err(|e| e.into()),
    }
}
