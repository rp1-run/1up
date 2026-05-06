use std::io::{self, Write};
use std::path::Path;

use anyhow::bail;
use clap::Args;

use crate::cli::{discovery_output, lean};
use crate::search::context::parse_location;
use crate::search::impact::{ImpactAnchor, ImpactHorizonEngine, ImpactRequest};
use crate::search::SearchScope;
use crate::shared::config::project_db_path;
use crate::storage::db::Db;
use crate::storage::schema;

#[derive(Args)]
pub struct ImpactArgs {
    /// Explore probable impact from a file or file:line anchor
    #[arg(long = "from-file")]
    pub from_file: Option<String>,

    /// Explore probable impact from a symbol definition anchor
    #[arg(long = "from-symbol")]
    pub from_symbol: Option<String>,

    /// Explore probable impact from a result handle
    #[arg(long = "from-handle")]
    pub from_handle: Option<String>,

    /// Explore probable impact from an exact segment identifier
    #[arg(long = "from-segment", hide = true)]
    pub from_segment: Option<String>,

    /// Limit expansion to a repo-relative subtree
    #[arg(long)]
    pub scope: Option<String>,

    /// Expansion depth (clamped to the supported horizon)
    #[arg(long, default_value = "2", value_parser = crate::cli::parse_positive_usize)]
    pub depth: usize,

    /// Maximum number of ranked candidates to return
    #[arg(long, short = 'n', default_value = "20", value_parser = crate::cli::parse_positive_usize)]
    pub limit: usize,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,

    /// Emit the stable lean output grammar instead of human-readable output
    #[arg(long)]
    pub plain: bool,
}

impl ImpactArgs {
    fn to_request(&self) -> anyhow::Result<ImpactRequest> {
        Ok(ImpactRequest {
            anchor: self.parse_anchor()?,
            scope: self.scope.clone(),
            depth: self.depth,
            limit: self.limit,
        })
    }

    fn parse_anchor(&self) -> anyhow::Result<ImpactAnchor> {
        let from_file = non_empty(self.from_file.as_deref());
        let from_symbol = non_empty(self.from_symbol.as_deref());
        let from_handle = non_empty(self.from_handle.as_deref());
        let from_segment = non_empty(self.from_segment.as_deref());

        let provided = from_file.is_some() as usize
            + from_symbol.is_some() as usize
            + from_handle.is_some() as usize
            + from_segment.is_some() as usize;

        if provided == 0 {
            bail!(
                "impact requires exactly one anchor; pass one of `--from-file`, `--from-symbol`, or `--from-handle`"
            );
        }

        if provided > 1 {
            bail!(
                "impact accepts exactly one anchor; choose only one of `--from-file`, `--from-symbol`, or `--from-handle`"
            );
        }

        if let Some(raw) = from_file {
            return parse_file_anchor(raw);
        }

        if let Some(name) = from_symbol {
            return Ok(ImpactAnchor::Symbol {
                name: name.to_string(),
            });
        }

        if let Some(handle) = from_handle {
            return Ok(ImpactAnchor::Segment {
                id: normalize_handle(handle),
            });
        }

        Ok(ImpactAnchor::Segment {
            id: normalize_handle(from_segment.expect("validated exactly one anchor")),
        })
    }
}

pub async fn exec(args: ImpactArgs) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let search_scope = SearchScope::from_worktree_context(&resolved.worktree_context);
    let db_path = project_db_path(&project_root);

    warn_if_degraded_branch_context(&search_scope);

    if !db_path.exists() {
        bail!(
            "no current index found at {}. Run `1up reindex` to create a fresh index.",
            db_path.display()
        );
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_current(&conn).await?;

    let engine = ImpactHorizonEngine::new_scoped(&conn, search_scope);
    let result = engine.explore(args.to_request()?).await?;

    let mut stdout = io::stdout().lock();
    if args.plain {
        lean::render_impact(&mut stdout, &result)?;
    } else {
        discovery_output::render_impact(&mut stdout, &result)?;
    }
    stdout.flush()?;
    Ok(())
}

fn warn_if_degraded_branch_context(scope: &SearchScope) {
    if let Some(reason) = scope.degraded_reason() {
        eprintln!("warning: {reason}");
    }
}

fn parse_file_anchor(raw: &str) -> anyhow::Result<ImpactAnchor> {
    if has_line_suffix(raw) {
        let (path, line) = parse_location(raw)?;
        return Ok(ImpactAnchor::File {
            path,
            line: Some(line),
        });
    }

    Ok(ImpactAnchor::File {
        path: raw.to_string(),
        line: None,
    })
}

fn has_line_suffix(raw: &str) -> bool {
    raw.rsplit_once(':')
        .map(|(_, suffix)| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
        .unwrap_or(false)
}

fn normalize_handle(raw: &str) -> String {
    raw.trim()
        .strip_prefix(':')
        .unwrap_or(raw.trim())
        .to_string()
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: ImpactArgs,
    }

    #[test]
    fn impact_args_require_an_anchor() {
        let args = ImpactArgs {
            from_file: None,
            from_symbol: None,
            from_handle: None,
            from_segment: None,
            scope: None,
            depth: 2,
            limit: 20,
            path: ".".to_string(),
            plain: false,
        };

        assert!(args.to_request().is_err());
    }

    #[test]
    fn impact_args_reject_multiple_anchors() {
        let args = ImpactArgs {
            from_file: Some("src/main.rs".to_string()),
            from_symbol: Some("Cli".to_string()),
            from_handle: None,
            from_segment: None,
            scope: None,
            depth: 2,
            limit: 20,
            path: ".".to_string(),
            plain: false,
        };

        assert!(args.to_request().is_err());
    }

    #[test]
    fn impact_args_parse_file_line_anchor() {
        let args = ImpactArgs {
            from_file: Some("src/main.rs:42".to_string()),
            from_symbol: None,
            from_handle: None,
            from_segment: None,
            scope: Some("src".to_string()),
            depth: 1,
            limit: 7,
            path: ".".to_string(),
            plain: false,
        };

        let request = args.to_request().unwrap();
        assert_eq!(
            request.anchor,
            ImpactAnchor::File {
                path: "src/main.rs".to_string(),
                line: Some(42)
            }
        );
        assert_eq!(request.scope.as_deref(), Some("src"));
        assert_eq!(request.depth, 1);
        assert_eq!(request.limit, 7);
    }

    #[test]
    fn impact_args_preserve_windows_style_paths_without_line_suffix() {
        let args = ImpactArgs {
            from_file: Some("C:/repo/src/main.rs".to_string()),
            from_symbol: None,
            from_handle: None,
            from_segment: None,
            scope: None,
            depth: 2,
            limit: 20,
            path: ".".to_string(),
            plain: false,
        };

        let request = args.to_request().unwrap();
        assert_eq!(
            request.anchor,
            ImpactAnchor::File {
                path: "C:/repo/src/main.rs".to_string(),
                line: None
            }
        );
    }

    #[test]
    fn impact_args_parse_public_handle_anchor() {
        let args = ImpactArgs {
            from_file: None,
            from_symbol: None,
            from_handle: Some(":abcdef012345".to_string()),
            from_segment: None,
            scope: None,
            depth: 2,
            limit: 20,
            path: ".".to_string(),
            plain: false,
        };

        let request = args.to_request().unwrap();
        assert_eq!(
            request.anchor,
            ImpactAnchor::Segment {
                id: "abcdef012345".to_string()
            }
        );
    }

    #[test]
    fn impact_args_parse_hidden_segment_alias() {
        let args = ImpactArgs {
            from_file: None,
            from_symbol: None,
            from_handle: None,
            from_segment: Some(":abcdef012345".to_string()),
            scope: None,
            depth: 2,
            limit: 20,
            path: ".".to_string(),
            plain: false,
        };

        let request = args.to_request().unwrap();
        assert_eq!(
            request.anchor,
            ImpactAnchor::Segment {
                id: "abcdef012345".to_string()
            }
        );
    }

    #[test]
    fn impact_clap_exposes_handle_and_hides_segment_alias() {
        let cli = TestCli::parse_from(["test", "--from-handle", ":abcdef012345", "--plain"]);
        assert_eq!(cli.args.from_handle.as_deref(), Some(":abcdef012345"));
        assert!(cli.args.plain);

        let mut command = TestCli::command();
        let help = command.render_help().to_string();
        assert!(help.contains("--from-handle"));
        assert!(!help.contains("--from-segment"));
    }
}
