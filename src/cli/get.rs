use std::io::{self, Write};
use std::path::Path;

use clap::Args;

use crate::cli::{discovery_output, lean};
use crate::shared::config::project_db_path;
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments::{
    get_segment_by_id, get_segment_by_prefix, SegmentPrefixLookup, StoredSegment,
};

/// Hydrate one or more segment handles to their full indexed record.
///
/// `get` is the fat companion to the lean discovery grammar: `search`, `symbol`, `impact`,
/// and `context` emit compact handles (`:<id>`) and `get` resolves those handles to the
/// body + metadata previously embedded in fat search output. Handles may be full
/// segment ids or the 12-char display prefix; both resolve through the same storage path.
///
/// Output per handle, in request order: `segment <id>` header, tab-separated metadata
/// line, blank line, body content, blank line, `---` sentinel terminator. Unknown
/// handles emit `not_found\t<id>` followed by `---`. Ambiguous prefixes render
/// `not_found` on stdout plus a disambiguation hint on stderr.
#[derive(Args)]
pub struct GetArgs {
    /// One or more segment handles to hydrate. Emission preserves argument order so
    /// callers can pipeline multi-hit follow-ups from a single `search`. Accepts
    /// either bare ids (`a0f1e2c3d4b5`) or lean-grammar handles with the leading
    /// colon (`:a0f1e2c3d4b5`); the 12-char display prefix is resolved against the
    /// full segment id via a storage-side prefix lookup.
    #[arg(required = true, num_args = 1..)]
    pub handles: Vec<String>,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,

    /// Emit the stable lean output grammar instead of human-readable output
    #[arg(long)]
    pub plain: bool,
}

pub async fn exec(args: GetArgs) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let db_path = project_db_path(&project_root);

    if !db_path.exists() {
        anyhow::bail!(
            "no current index found at {}. Run `1up reindex` to create a fresh index.",
            db_path.display()
        );
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_current(&conn).await?;

    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();

    for raw_handle in &args.handles {
        let handle = normalize_handle(raw_handle);
        let outcome = resolve_handle(&conn, &handle).await?;
        write_outcome(
            &mut stdout,
            &mut stderr,
            raw_handle,
            &handle,
            &outcome,
            args.plain,
        )?;
    }

    stdout.flush()?;
    Ok(())
}

/// Strip the leading `:` that the lean grammar emits, so agents can paste
/// `1up get :a0f1e2` without trimming first.
fn normalize_handle(raw: &str) -> String {
    raw.strip_prefix(':').unwrap_or(raw).to_string()
}

/// Outcome of resolving a single handle, before any rendering happens.
///
/// Captured as an enum instead of writing directly so T4's `lean::render_get` can
/// consume these values once the renderer module lands.
#[derive(Debug)]
enum HandleOutcome {
    Found(Box<StoredSegment>),
    NotFound,
    Ambiguous(Vec<String>),
}

async fn resolve_handle(conn: &libsql::Connection, handle: &str) -> anyhow::Result<HandleOutcome> {
    if handle.is_empty() {
        return Ok(HandleOutcome::NotFound);
    }

    if let Some(segment) = get_segment_by_id(conn, handle).await? {
        return Ok(HandleOutcome::Found(Box::new(segment)));
    }

    Ok(match get_segment_by_prefix(conn, handle).await? {
        SegmentPrefixLookup::Found(segment) => HandleOutcome::Found(segment),
        SegmentPrefixLookup::NotFound => HandleOutcome::NotFound,
        SegmentPrefixLookup::Ambiguous(ids) => HandleOutcome::Ambiguous(ids),
    })
}

fn write_outcome<W: Write, E: Write>(
    out: &mut W,
    err: &mut E,
    raw_handle: &str,
    handle: &str,
    outcome: &HandleOutcome,
    plain: bool,
) -> anyhow::Result<()> {
    match outcome {
        HandleOutcome::Found(segment) if plain => lean::render_get_found(out, segment)?,
        HandleOutcome::Found(segment) => discovery_output::render_get_found(out, segment)?,
        HandleOutcome::NotFound if plain => lean::render_get_not_found(out, raw_handle)?,
        HandleOutcome::NotFound => discovery_output::render_get_not_found(out, raw_handle)?,
        HandleOutcome::Ambiguous(ids) => {
            if plain {
                lean::render_get_not_found(out, raw_handle)?;
            } else {
                discovery_output::render_get_not_found(out, raw_handle)?;
            }
            writeln!(
                err,
                "handle `{handle}` matched {} segments: {}. Provide a longer prefix.",
                ids.len(),
                ids.join(", ")
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: GetArgs,
    }

    #[test]
    fn normalize_strips_leading_colon() {
        assert_eq!(normalize_handle(":a0f1e2"), "a0f1e2");
        assert_eq!(normalize_handle("a0f1e2"), "a0f1e2");
        assert_eq!(normalize_handle(""), "");
    }

    #[test]
    fn get_args_require_at_least_one_handle() {
        let parsed = TestCli::try_parse_from(["test"]);
        assert!(parsed.is_err(), "expected clap to reject zero handles");
    }

    #[test]
    fn get_args_accept_multiple_handles_in_order() {
        let cli = TestCli::parse_from(["test", "a0f1", "b7c2", ":c4d5"]);
        assert_eq!(cli.args.handles, vec!["a0f1", "b7c2", ":c4d5"]);
    }

    #[test]
    fn get_args_accept_plain_mode() {
        let cli = TestCli::parse_from(["test", "a0f1", "--plain"]);
        assert!(cli.args.plain);
    }
}
