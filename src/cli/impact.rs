use std::io::{self, Write};
use std::path::Path;

use anyhow::bail;
use clap::Args;

use crate::cli::lean;
use crate::search::context::parse_location;
use crate::search::impact::{ImpactAnchor, ImpactHorizonEngine, ImpactRequest};
use crate::shared::config::project_db_path;
use crate::shared::types::OutputFormat;
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

    /// Explore probable impact from an exact segment identifier
    #[arg(long = "from-segment")]
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
        let provided = self.from_file.is_some() as usize
            + self.from_symbol.is_some() as usize
            + self.from_segment.is_some() as usize;

        if provided == 0 {
            bail!(
                "impact requires exactly one anchor; pass one of `--from-file`, `--from-symbol`, or `--from-segment`"
            );
        }

        if provided > 1 {
            bail!(
                "impact accepts exactly one anchor; choose only one of `--from-file`, `--from-symbol`, or `--from-segment`"
            );
        }

        if let Some(raw) = &self.from_file {
            return parse_file_anchor(raw);
        }

        if let Some(name) = &self.from_symbol {
            return Ok(ImpactAnchor::Symbol { name: name.clone() });
        }

        Ok(ImpactAnchor::Segment {
            id: self
                .from_segment
                .clone()
                .expect("validated exactly one anchor"),
        })
    }
}

pub async fn exec(args: ImpactArgs, _format: OutputFormat) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let db_path = project_db_path(&project_root);

    if !db_path.exists() {
        bail!(
            "no current index found at {}. Run `1up reindex` to create a fresh index.",
            db_path.display()
        );
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_current(&conn).await?;

    let engine = ImpactHorizonEngine::new(&conn);
    let result = engine.explore(args.to_request()?).await?;

    let mut stdout = io::stdout().lock();
    lean::render_impact(&mut stdout, &result)?;
    stdout.flush()?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impact_args_require_an_anchor() {
        let args = ImpactArgs {
            from_file: None,
            from_symbol: None,
            from_segment: None,
            scope: None,
            depth: 2,
            limit: 20,
            path: ".".to_string(),
        };

        assert!(args.to_request().is_err());
    }

    #[test]
    fn impact_args_reject_multiple_anchors() {
        let args = ImpactArgs {
            from_file: Some("src/main.rs".to_string()),
            from_symbol: Some("Cli".to_string()),
            from_segment: None,
            scope: None,
            depth: 2,
            limit: 20,
            path: ".".to_string(),
        };

        assert!(args.to_request().is_err());
    }

    #[test]
    fn impact_args_parse_file_line_anchor() {
        let args = ImpactArgs {
            from_file: Some("src/main.rs:42".to_string()),
            from_symbol: None,
            from_segment: None,
            scope: Some("src".to_string()),
            depth: 1,
            limit: 7,
            path: ".".to_string(),
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
            from_segment: None,
            scope: None,
            depth: 2,
            limit: 20,
            path: ".".to_string(),
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
}
