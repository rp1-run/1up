use std::io::{self, Write};
use std::path::Path;

use clap::Args;

use crate::shared::config::project_db_path;
use crate::shared::types::OutputFormat;
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments::{
    get_segment_by_id, get_segment_by_prefix, SegmentPrefixLookup, StoredSegment,
};

/// Hydrate one or more segment handles to their full indexed record.
///
/// `get` is the fat companion to the lean discovery grammar: `search`, `symbol`, `impact`,
/// and `context` emit compact handles (`:<id>`) and `get` resolves those handles to the
/// body + metadata previously embedded in fat search output. Handles may be full 16-char
/// segment ids or the 12-char display prefix; both resolve through the same storage path.
#[derive(Args)]
pub struct GetArgs {
    /// One or more segment handles to hydrate, in the order they should be emitted.
    /// Accepts bare ids (`a0f1e2c3d4b5`) or handles with the leading colon produced by
    /// the lean row grammar (`:a0f1e2c3d4b5`).
    #[arg(required = true, num_args = 1..)]
    pub handles: Vec<String>,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: GetArgs, _format: OutputFormat) -> anyhow::Result<()> {
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
        write_outcome(&mut stdout, &mut stderr, raw_handle, &handle, &outcome)?;
    }

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
) -> anyhow::Result<()> {
    match outcome {
        HandleOutcome::Found(segment) => write_segment(out, segment)?,
        HandleOutcome::NotFound => {
            writeln!(out, "not_found\t{raw_handle}")?;
            writeln!(out, "---")?;
        }
        HandleOutcome::Ambiguous(ids) => {
            writeln!(out, "not_found\t{raw_handle}")?;
            writeln!(out, "---")?;
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

fn write_segment<W: Write>(out: &mut W, segment: &StoredSegment) -> anyhow::Result<()> {
    writeln!(out, "segment {}", segment.id)?;

    let defined = segment.parsed_defined_symbols().join(",");
    let referenced = segment.parsed_referenced_symbols().join(",");
    let called = segment.parsed_called_symbols().join(",");
    let breadcrumb = segment.breadcrumb.as_deref().unwrap_or("-");

    writeln!(
        out,
        "path\t{}\tlines\t{}-{}\tkind\t{}\tlanguage\t{}\tbreadcrumb\t{}\trole\t{}\tcomplexity\t{}\tdefines\t{}\treferences\t{}\tcalls\t{}",
        segment.file_path,
        segment.line_start,
        segment.line_end,
        segment.block_type,
        segment.language,
        breadcrumb,
        role_label(&segment.role),
        segment.complexity,
        defined,
        referenced,
        called,
    )?;
    writeln!(out)?;
    writeln!(out, "{}", segment.content)?;
    writeln!(out)?;
    writeln!(out, "---")?;
    Ok(())
}

fn role_label(role: &str) -> String {
    role.to_ascii_lowercase()
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
}
