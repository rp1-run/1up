pub mod add_mcp;
pub mod context;
pub mod get;
pub mod impact;
pub mod index;
pub mod init;
pub mod lean;
pub mod list;
pub mod mcp;
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
    about = "🍄 1up — Manage the local project lifecycle",
    help_template = "\
{about}

Usage: {usage}

Commands:
  start   Start or refresh the local project lifecycle
  status  Show project lifecycle, daemon, and index state
  list    List registered 1up projects
  stop    Stop lifecycle activity for a project

Options:
{options}",
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
    /// Start or refresh the local project lifecycle
    Start(start::StartArgs),

    /// Show project lifecycle, daemon, and index state
    Status(status::StatusArgs),

    /// List registered 1up projects
    List(list::ListArgs),

    /// Stop lifecycle activity for a project
    Stop(stop::StopArgs),

    /// Add the local 1up MCP server to an agent host through add-mcp
    #[command(hide = true)]
    AddMcp(add_mcp::AddMcpArgs),

    /// Initialize a project for 1up indexing
    #[command(hide = true)]
    Init(init::InitArgs),

    /// Look up symbol definitions and references. Emits one lean row per hit
    /// (`<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`).
    #[command(hide = true)]
    Symbol(symbol::SymbolArgs),

    /// Hybrid semantic + full-text search. Emits one lean row per hit
    /// (`<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`);
    /// defaults to top-3. Pair with `1up get <id>` to hydrate bodies.
    #[command(hide = true)]
    Search(search::SearchArgs),

    /// Hydrate one or more segment handles to their full indexed record. Emits
    /// `segment <id>` header, tab-separated metadata, blank line, body, `---`
    /// sentinel per handle in request order; unknown handles emit `not_found`.
    #[command(hide = true)]
    Get(get::GetArgs),

    /// Retrieve code context around a file location. Emits
    /// `<path>:<l1>-<l2>  context  <scope_type>` header followed by numbered
    /// body lines; no `:<segment_id>` suffix because context is read-after-pick.
    #[command(hide = true)]
    Context(context::ContextArgs),

    /// Explore probable impact from a known anchor. Emits lean rows with a
    /// trailing `~P` (primary) or `~C` (contextual) channel tag; `refused`,
    /// `empty`, and `empty_scoped` envelopes render as terminal status lines.
    #[command(hide = true)]
    Impact(impact::ImpactArgs),

    /// Structural AST-pattern search using tree-sitter queries. Emits one lean
    /// row per match (`<path>:<l1>-<l2>  structural  <language>::<pattern_name>`)
    /// followed by the indented snippet.
    #[command(hide = true)]
    Structural(structural::StructuralArgs),

    /// Start the MCP stdio server for agent-facing code discovery
    #[command(hide = true)]
    Mcp(mcp::McpArgs),

    /// Index a repository
    #[command(hide = true)]
    Index(index::IndexArgs),

    /// Force re-index of all files
    #[command(hide = true)]
    Reindex(reindex::ReindexArgs),

    /// Check for updates, view update status, or apply an update
    #[command(hide = true)]
    Update(update::UpdateArgs),

    /// Internal: daemon worker process (not for direct use)
    #[command(name = "__worker", hide = true)]
    Worker,
}

impl Command {
    /// Default output format for maintenance commands when `--format`/`-f` is
    /// not supplied. Returns `None` for core commands and the internal worker,
    /// which own their output protocol directly and never consult a format.
    pub fn default_maintenance_format(&self) -> Option<OutputFormat> {
        match self {
            Command::Start(_) | Command::Status(_) | Command::List(_) | Command::Stop(_) => {
                Some(OutputFormat::Human)
            }
            Command::Update(_) => Some(OutputFormat::Human),
            Command::Init(_) | Command::Index(_) | Command::Reindex(_) => Some(OutputFormat::Plain),
            Command::AddMcp(_)
            | Command::Search(_)
            | Command::Get(_)
            | Command::Symbol(_)
            | Command::Context(_)
            | Command::Impact(_)
            | Command::Structural(_)
            | Command::Mcp(_)
            | Command::Worker => None,
        }
    }
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    // Resolve the maintenance format (if any) before moving out of `cli.command`.
    // Core commands own their output protocol directly and never consult a
    // format; their dispatch arms below take no format argument.
    let maintenance_format = cli.command.default_maintenance_format();
    match cli.command {
        Command::AddMcp(args) => add_mcp::exec(args).await,
        Command::Init(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            init::exec(args, format).await
        }
        Command::Start(args) => {
            let format = resolve_lifecycle_format(args.plain, args.format);
            start::exec(args, format).await
        }
        Command::Status(args) => {
            let format = resolve_lifecycle_format(args.plain, args.format);
            status::exec(args, format).await
        }
        Command::List(args) => {
            let format = resolve_lifecycle_format(args.plain, args.format);
            list::exec(args, format).await
        }
        Command::Stop(args) => {
            let format = resolve_lifecycle_format(args.plain, args.format);
            stop::exec(args, format).await
        }
        Command::Symbol(args) => symbol::exec(args).await,
        Command::Search(args) => search::exec(args).await,
        Command::Get(args) => get::exec(args).await,
        Command::Context(args) => context::exec(args).await,
        Command::Impact(args) => impact::exec(args).await,
        Command::Structural(args) => structural::exec(args).await,
        Command::Mcp(args) => mcp::exec(args).await,
        Command::Index(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            index::exec(args, format).await
        }
        Command::Reindex(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            reindex::exec(args, format).await
        }
        Command::Update(args) => {
            let format = resolve_maintenance_format(args.format, maintenance_format);
            update::exec(args, format).await
        }
        Command::Worker => crate::daemon::worker::run().await.map_err(|e| e.into()),
    }
}

fn resolve_lifecycle_format(plain: bool, explicit: Option<OutputFormat>) -> OutputFormat {
    if plain {
        OutputFormat::Plain
    } else {
        resolve_maintenance_format(explicit, Some(OutputFormat::Human))
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
    use clap::CommandFactory;

    /// Core commands (`search`, `get`, `symbol`, `impact`, `context`,
    /// `structural`, `mcp`) must not accept any presentation flag. Verifies at clap
    /// parse time that `-f`/`--format` (and variants like `--human`/`--full`)
    /// are rejected as unknown arguments so agents get a clean signal, not
    /// silent coercion.
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
            &["1up", "mcp", "--format", "json"],
            &["1up", "mcp", "-f", "json"],
            &["1up", "add-mcp", "--format", "json"],
            &["1up", "add-mcp", "-f", "json"],
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
    /// `reindex`, `update`) must still accept `--format`/`-f`
    /// so existing scripting and JSON-consuming integrations keep working.
    #[test]
    fn maintenance_commands_still_accept_format() {
        let maintenance_cases: &[&[&str]] = &[
            &["1up", "status", ".", "--format", "json"],
            &["1up", "status", ".", "-f", "json"],
            &["1up", "start", ".", "--format", "human"],
            &["1up", "list", "--format", "json"],
            &["1up", "list", "-f", "json"],
            &["1up", "stop", ".", "--format", "json"],
            &["1up", "init", ".", "--format", "json"],
            &["1up", "index", ".", "--format", "json"],
            &["1up", "reindex", ".", "--format", "json"],
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
            &["1up", "status", "."],
            &["1up", "list"],
            &["1up", "stop", "."],
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
            &["1up", "mcp"],
            &["1up", "add-mcp"],
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

    #[test]
    fn top_level_help_shows_only_p1_lifecycle_commands() {
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        let visible_commands = ["start", "status", "list", "stop"];

        for command_name in visible_commands {
            assert!(
                help.lines()
                    .any(|line| line.trim_start().starts_with(command_name)),
                "expected top-level help to show {command_name}; help was:\n{help}",
            );
        }

        let hidden_commands = [
            "add-mcp",
            "init",
            "symbol",
            "search",
            "get",
            "context",
            "impact",
            "structural",
            "mcp",
            "index",
            "reindex",
            "update",
            "__worker",
            "hello-agent",
            "help",
        ];

        for command_name in hidden_commands {
            assert!(
                !help
                    .lines()
                    .any(|line| line.trim_start().starts_with(command_name)),
                "expected top-level help to hide {command_name}; help was:\n{help}",
            );
        }
    }

    #[test]
    fn lifecycle_command_help_documents_plain_and_hides_format() {
        for subcommand in ["start", "status", "list", "stop"] {
            let mut command = Cli::command();
            let subcommand = command
                .find_subcommand_mut(subcommand)
                .expect("lifecycle subcommand should exist");
            let help = subcommand.render_help().to_string();

            assert!(
                help.contains("--plain"),
                "expected lifecycle help to show --plain; help was:\n{help}",
            );
            assert!(
                !help.contains("--format") && !help.contains("-f,"),
                "expected lifecycle help to hide --format/-f; help was:\n{help}",
            );
            assert!(
                !help.contains("add-mcp") && !help.contains("Homebrew") && !help.contains("Scoop"),
                "expected lifecycle help to avoid removed setup/package surfaces; help was:\n{help}",
            );
        }
    }

    #[test]
    fn lifecycle_plain_conflicts_with_hidden_format() {
        for subcommand in ["start", "status", "list", "stop"] {
            let plain = Cli::try_parse_from(["1up", subcommand, "--plain"]);
            assert!(
                plain.is_ok(),
                "lifecycle command rejected --plain: {subcommand} -> {:?}",
                plain.err()
            );

            for format_flag in ["--format", "-f"] {
                let result =
                    Cli::try_parse_from(["1up", subcommand, "--plain", format_flag, "json"]);
                assert!(
                    result.is_err(),
                    "lifecycle command accepted conflicting --plain and {format_flag}: {subcommand}",
                );
            }
        }
    }

    #[test]
    fn lifecycle_format_resolver_prefers_plain_then_hidden_format_then_human_default() {
        assert_eq!(resolve_lifecycle_format(false, None), OutputFormat::Human);
        assert_eq!(resolve_lifecycle_format(true, None), OutputFormat::Plain);
        assert_eq!(
            resolve_lifecycle_format(false, Some(OutputFormat::Plain)),
            OutputFormat::Plain
        );
        assert_eq!(
            resolve_lifecycle_format(false, Some(OutputFormat::Json)),
            OutputFormat::Json
        );
    }
}
