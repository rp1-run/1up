use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::cli::output::formatter_for;
use crate::daemon::lifecycle;
use crate::search::context::{parse_location, ContextEngine};
use crate::shared::errors::{FilesystemError, OneupError};
use crate::shared::fs::clamp_canonical_path_to_root;
use crate::shared::project;
use crate::shared::types::{ContextAccessScope, OutputFormat};

#[derive(Args)]
pub struct ContextArgs {
    /// File location in file:line format
    pub location: String,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,

    /// Context expansion window in lines (used when tree-sitter scope unavailable)
    #[arg(long)]
    pub expansion: Option<usize>,

    /// Allow reading a canonicalized target outside the selected project root
    #[arg(long)]
    pub allow_outside_root: bool,
}

pub async fn exec(args: ContextArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);

    if let Ok(pid) = project::read_project_id(&project_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, &project_root) {
            tracing::debug!("auto-start daemon skipped: {e}");
        }
    }

    let (file_str, line) = parse_location(&args.location)?;
    let (file_path, access_scope) =
        resolve_context_target(&project_root, &file_str, args.allow_outside_root)?;

    let result = match access_scope {
        ContextAccessScope::ProjectRoot => {
            ContextEngine::retrieve(&file_path, line, args.expansion)?
        }
        ContextAccessScope::OutsideRoot => {
            ContextEngine::retrieve_with_scope(&file_path, line, args.expansion, access_scope)?
        }
    };
    println!("{}", fmt.format_context_result(&result));
    Ok(())
}

fn resolve_context_target(
    project_root: &Path,
    requested_file: &str,
    allow_outside_root: bool,
) -> anyhow::Result<(PathBuf, ContextAccessScope)> {
    let requested = Path::new(requested_file);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        project_root.join(requested)
    };

    let canonical_candidate = match candidate.canonicalize() {
        Ok(path) => path,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            anyhow::bail!("file not found: {}", candidate.display());
        }
        Err(err) => return Err(err.into()),
    };

    let canonical_target = if allow_outside_root {
        canonical_candidate
    } else {
        match clamp_canonical_path_to_root(project_root, &canonical_candidate) {
            Ok(path) => path,
            Err(OneupError::Filesystem(FilesystemError::OutsideApprovedRoot { .. })) => {
                anyhow::bail!(
                    "context target is outside the selected project root; rerun with --allow-outside-root to read {}",
                    candidate.display()
                );
            }
            Err(err) => return Err(anyhow::Error::new(err)),
        }
    };

    let access_scope = if canonical_target.starts_with(project_root) {
        ContextAccessScope::ProjectRoot
    } else {
        ContextAccessScope::OutsideRoot
    };

    Ok((canonical_target, access_scope))
}
