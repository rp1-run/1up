pub mod context;
pub mod hello_agent;
pub mod index;
pub mod init;
pub mod output;
pub mod reindex;
pub mod search;
pub mod start;
pub mod status;
pub mod stop;
pub mod structural;
pub mod symbol;
pub mod update;

use clap::{Parser, Subcommand};

use crate::shared::types::OutputFormat;

#[derive(Parser)]
#[command(
    name = "🍄 1up",
    about = "🍄 1up — Unified search substrate for source repositories",
    version,
    propagate_version = true
)]
pub struct Cli {
    /// Output format override. Defaults to human for start/status/stop/update/hello-agent; plain otherwise.
    #[arg(long, short, global = true)]
    pub format: Option<OutputFormat>,

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

    /// Initialize if needed, index, and start the background daemon
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

    /// Structural AST-pattern search using tree-sitter queries
    Structural(structural::StructuralArgs),

    /// Index a repository
    Index(index::IndexArgs),

    /// Force re-index of all files
    Reindex(reindex::ReindexArgs),

    /// Output a concise agent instruction for AI assistants
    HelloAgent(hello_agent::HelloAgentArgs),

    /// Check for updates, view update status, or apply an update
    Update(update::UpdateArgs),

    /// Internal: daemon worker process (not for direct use)
    #[command(name = "__worker", hide = true)]
    Worker,
}

impl Cli {
    pub fn resolved_format(&self) -> OutputFormat {
        self.format.unwrap_or_else(|| self.command.default_format())
    }
}

impl Command {
    pub fn default_format(&self) -> OutputFormat {
        match self {
            Command::Start(_)
            | Command::Stop(_)
            | Command::Status(_)
            | Command::HelloAgent(_)
            | Command::Update(_) => OutputFormat::Human,
            Command::Init(_)
            | Command::Search(_)
            | Command::Symbol(_)
            | Command::Context(_)
            | Command::Structural(_)
            | Command::Index(_)
            | Command::Reindex(_)
            | Command::Worker => OutputFormat::Plain,
        }
    }
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let format = cli.resolved_format();
    match cli.command {
        Command::Init(args) => init::exec(args, format).await,
        Command::Start(args) => start::exec(args, format).await,
        Command::Stop(args) => stop::exec(args, format).await,
        Command::Status(args) => status::exec(args, format).await,
        Command::Symbol(args) => symbol::exec(args, format).await,
        Command::Search(args) => search::exec(args, format).await,
        Command::Context(args) => context::exec(args, format).await,
        Command::Structural(args) => structural::exec(args, format).await,
        Command::Index(args) => index::exec(args, format).await,
        Command::Reindex(args) => reindex::exec(args, format).await,
        Command::HelloAgent(args) => hello_agent::exec(args, format).await,
        Command::Update(args) => update::exec(args, format).await,
        Command::Worker => crate::daemon::worker::run().await.map_err(|e| e.into()),
    }
}

pub(crate) fn parse_positive_usize(raw: &str) -> Result<usize, String> {
    let parsed = raw
        .parse::<usize>()
        .map_err(|_| format!("invalid positive integer: {raw}"))?;

    if parsed == 0 {
        return Err(format!("value must be at least 1, got {raw}"));
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_format_defaults_lifecycle_commands_to_human() {
        for argv in [
            &["1up", "start", "."][..],
            &["1up", "stop", "."][..],
            &["1up", "status", "."][..],
            &["1up", "hello-agent"][..],
            &["1up", "update", "--status"][..],
        ] {
            let cli = Cli::parse_from(argv);
            assert_eq!(cli.resolved_format(), OutputFormat::Human);
        }
    }

    #[test]
    fn resolved_format_keeps_search_and_index_commands_plain() {
        for argv in [
            &["1up", "search", "needle"][..],
            &["1up", "symbol", "Config"][..],
            &["1up", "context", "src/main.rs:1"][..],
            &["1up", "structural", "(identifier) @id"][..],
            &["1up", "init", "."][..],
            &["1up", "index", "."][..],
            &["1up", "reindex", "."][..],
        ] {
            let cli = Cli::parse_from(argv);
            assert_eq!(cli.resolved_format(), OutputFormat::Plain);
        }
    }

    #[test]
    fn resolved_format_prefers_explicit_override() {
        let cli = Cli::parse_from(["1up", "--format", "json", "status", "."]);
        assert_eq!(cli.resolved_format(), OutputFormat::Json);
    }
}
