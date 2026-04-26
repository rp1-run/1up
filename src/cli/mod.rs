pub mod context;
pub mod get;
pub mod hello_agent;
pub mod impact;
pub mod index;
pub mod init;
pub mod lean;
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

    /// Look up symbol definitions and references. Emits one lean row per hit
    /// (`<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`).
    Symbol(symbol::SymbolArgs),

    /// Hybrid semantic + full-text search. Emits one lean row per hit
    /// (`<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`);
    /// defaults to top-3. Pair with `1up get <id>` to hydrate bodies.
    Search(search::SearchArgs),

    /// Hydrate one or more segment handles to their full indexed record. Emits
    /// `segment <id>` header, tab-separated metadata, blank line, body, `---`
    /// sentinel per handle in request order; unknown handles emit `not_found`.
    Get(get::GetArgs),

    /// Retrieve code context around a file location. Emits
    /// `<path>:<l1>-<l2>  context  <scope_type>` header followed by numbered
    /// body lines; no `:<segment_id>` suffix because context is read-after-pick.
    Context(context::ContextArgs),

    /// Explore probable impact from a known anchor. Emits lean rows with a
    /// trailing `~P` (primary) or `~C` (contextual) channel tag; `refused`,
    /// `empty`, and `empty_scoped` envelopes render as terminal status lines.
    Impact(impact::ImpactArgs),

    /// Structural AST-pattern search using tree-sitter queries. Emits one lean
    /// row per match (`<path>:<l1>-<l2>  structural  <language>::<pattern_name>`)
    /// followed by the indented snippet.
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

impl Command {
    /// Default output format for maintenance commands when `--format`/`-f` is
    /// not supplied. Returns `None` for core commands and the internal worker,
    /// which render through the lean grammar (or do not render at all) and
    /// never consult a format.
    pub fn default_maintenance_format(&self) -> Option<OutputFormat> {
        match self {
            Command::Start(_)
            | Command::Stop(_)
            | Command::Status(_)
            | Command::HelloAgent(_)
            | Command::Update(_) => Some(OutputFormat::Human),
            Command::Init(_) | Command::Index(_) | Command::Reindex(_) => Some(OutputFormat::Plain),
            Command::Search(_)
            | Command::Get(_)
            | Command::Symbol(_)
            | Command::Context(_)
            | Command::Impact(_)
            | Command::Structural(_)
            | Command::Worker => None,
        }
    }
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    // Resolve the maintenance format (if any) before moving out of `cli.command`.
    // Core commands render through the lean grammar directly and never consult
    // a format; their dispatch arms below take no format argument.
    let maintenance_format = cli.command.default_maintenance_format();
    match cli.command {
        Command::Init(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            init::exec(args, format).await
        }
        Command::Start(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            start::exec(args, format).await
        }
        Command::Stop(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            stop::exec(args, format).await
        }
        Command::Status(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            status::exec(args, format).await
        }
        Command::Symbol(args) => symbol::exec(args).await,
        Command::Search(args) => search::exec(args).await,
        Command::Get(args) => get::exec(args).await,
        Command::Context(args) => context::exec(args).await,
        Command::Impact(args) => impact::exec(args).await,
        Command::Structural(args) => structural::exec(args).await,
        Command::Index(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            index::exec(args, format).await
        }
        Command::Reindex(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            reindex::exec(args, format).await
        }
        Command::HelloAgent(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            hello_agent::exec(args, format).await
        }
        Command::Update(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            update::exec(args, format).await
        }
        Command::Worker => crate::daemon::worker::run().await.map_err(|e| e.into()),
    }
}

/// Collapse the user-selected `--format` (if any) against the maintenance
/// default resolved from the command enum. Maintenance arms always have a
/// concrete default; the `.expect` documents that invariant so a future
/// refactor that accidentally classifies a core command as maintenance fails
/// loudly instead of silently picking `Plain`.
fn resolve_maintenance_format(
    explicit: Option<OutputFormat>,
    default: Option<OutputFormat>,
) -> OutputFormat {
    explicit
        .or(default)
        .expect("maintenance dispatch arms always resolve a default format")
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

    /// Core commands (`search`, `get`, `symbol`, `impact`, `context`,
    /// `structural`) must not accept any presentation flag. Verifies at clap
    /// parse time that `-f`/`--format` (and variants like `--human`/`--full`)
    /// are rejected as unknown arguments — agents relying on the lean grammar
    /// get a clean signal, not silent coercion.
    #[test]
    fn core_commands_have_no_format_arg() {
        let core_cases: &[&[&str]] = &[
            &["1up", "search", "needle", "--format", "json"],
            &["1up", "search", "needle", "-f", "json"],
            &["1up", "symbol", "Config", "--format", "json"],
            &["1up", "symbol", "Config", "-f", "json"],
            &["1up", "get", "abc123def456", "--format", "json"],
            &["1up", "get", "abc123def456", "-f", "json"],
            &["1up", "context", "src/main.rs:1", "--format", "json"],
            &["1up", "context", "src/main.rs:1", "-f", "json"],
            &[
                "1up",
                "impact",
                "--from-symbol",
                "Config",
                "--format",
                "json",
            ],
            &["1up", "impact", "--from-symbol", "Config", "-f", "json"],
            &["1up", "structural", "(identifier) @id", "--format", "json"],
            &["1up", "structural", "(identifier) @id", "-f", "json"],
        ];
        for argv in core_cases {
            let result = Cli::try_parse_from(argv.iter().copied());
            assert!(
                result.is_err(),
                "core command accepted a format flag: {argv:?}"
            );
        }
    }

    /// Maintenance commands (`start`, `stop`, `status`, `init`, `index`,
    /// `reindex`, `update`, `hello-agent`) must still accept `--format`/`-f`
    /// so existing scripting and JSON-consuming integrations keep working.
    #[test]
    fn maintenance_commands_still_accept_format() {
        let maintenance_cases: &[&[&str]] = &[
            &["1up", "status", ".", "--format", "json"],
            &["1up", "status", ".", "-f", "json"],
            &["1up", "start", ".", "--format", "human"],
            &["1up", "stop", ".", "--format", "json"],
            &["1up", "init", ".", "--format", "json"],
            &["1up", "index", ".", "--format", "json"],
            &["1up", "reindex", ".", "--format", "json"],
            &["1up", "hello-agent", "--format", "plain"],
            &["1up", "update", "--status", "--format", "json"],
        ];
        for argv in maintenance_cases {
            let result = Cli::try_parse_from(argv.iter().copied());
            assert!(
                result.is_ok(),
                "maintenance command rejected format flag: {argv:?} -> {:?}",
                result.err()
            );
        }
    }

    #[test]
    fn maintenance_format_defaults_match_prior_dispatch() {
        let human_defaults: &[&[&str]] = &[
            &["1up", "start", "."],
            &["1up", "stop", "."],
            &["1up", "status", "."],
            &["1up", "hello-agent"],
            &["1up", "update", "--status"],
        ];
        for argv in human_defaults {
            let cli = Cli::parse_from(argv.iter().copied());
            assert_eq!(
                cli.command.default_maintenance_format(),
                Some(OutputFormat::Human),
                "expected Human default for {argv:?}"
            );
        }

        let plain_defaults: &[&[&str]] = &[
            &["1up", "init", "."],
            &["1up", "index", "."],
            &["1up", "reindex", "."],
        ];
        for argv in plain_defaults {
            let cli = Cli::parse_from(argv.iter().copied());
            assert_eq!(
                cli.command.default_maintenance_format(),
                Some(OutputFormat::Plain),
                "expected Plain default for {argv:?}"
            );
        }
    }

    /// Core commands no longer resolve a maintenance format; the helper
    /// returns `None` so a refactor that accidentally routes a core command
    /// through maintenance dispatch panics via the `.expect` in
    /// `resolve_maintenance_format`.
    #[test]
    fn core_commands_have_no_maintenance_format_default() {
        let core_cases: &[&[&str]] = &[
            &["1up", "search", "needle"],
            &["1up", "symbol", "Config"],
            &["1up", "get", "abc123def456"],
            &["1up", "context", "src/main.rs:1"],
            &["1up", "impact", "--from-symbol", "Config"],
            &["1up", "structural", "(identifier) @id"],
        ];
        for argv in core_cases {
            let cli = Cli::parse_from(argv.iter().copied());
            assert_eq!(
                cli.command.default_maintenance_format(),
                None,
                "expected no maintenance default for {argv:?}"
            );
        }
    }
}
