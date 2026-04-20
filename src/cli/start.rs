use std::path::Path;
use std::time::Instant;

use clap::Args;

use crate::cli::output::{formatter_for, Formatter};
use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::constants;
use crate::shared::project;
use crate::shared::reminder;
use crate::shared::types::{IndexingConfig, OutputFormat, SetupTimings};
use crate::storage::db::Db;
use crate::storage::schema;

/// Classification of an existing project's on-disk index state.
///
/// Produced by [`classify_project_index`] and consumed by `1up start` to
/// decide whether to (a) proceed with indexing, (b) warn the user that the
/// schema is stale and point at `1up reindex` (REQ-033, BR-06), or
/// (c) warn that the index is unreadable and needs a reindex.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectIndexState {
    /// `.1up/index.db` does not exist yet. Indexing will create it.
    NotCreated,
    /// Schema matches the current `SCHEMA_VERSION`.
    Current,
    /// An older schema version is on disk.
    OutOfDate { found: u32, expected: u32 },
    /// The database file exists but its schema could not be determined
    /// (missing schema metadata, corrupt file, newer-than-supported
    /// version, or equivalent).
    UnknownUnreadable,
}

#[derive(Args)]
pub struct StartArgs {
    /// Project root directory (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Maximum concurrent parse workers; overrides ONEUP_INDEX_JOBS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub jobs: Option<usize>,

    /// ONNX intra-op threads; overrides ONEUP_EMBED_THREADS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub embed_threads: Option<usize>,

    /// Output format override (defaults to human)
    #[arg(long, short = 'f')]
    pub format: Option<OutputFormat>,
}

pub async fn exec(args: StartArgs, format: OutputFormat) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let source_root = resolved.source_root;
    let fmt = formatter_for(format);

    install_fences(&source_root, &*fmt);

    if !lifecycle::supports_daemon() {
        println!(
            "{}",
            fmt.format_message(
                "Background daemon workflows are not supported on this platform yet. Use `1up init` and `1up index`, then rerun `1up index` or `1up reindex` to refresh the local database."
            )
        );
        return Ok(());
    }

    let mut registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;

    let (project_id, initialized_now) = if project::is_initialized(&project_root) {
        (project::read_project_id(&project_root)?, false)
    } else {
        let id = project::write_project_id(&project_root)?;
        tracing::info!(
            "initialized project {} at {} during start",
            id,
            project_root.display()
        );
        (id, true)
    };
    let init_prefix = if initialized_now {
        format!("Initialized project {project_id}. ")
    } else {
        String::new()
    };

    // Classify the on-disk index before deciding the indexing branch. This
    // gives stale-schema users a concrete `1up reindex` message (REQ-033)
    // instead of an opaque migration error bubbling up from the indexer.
    let index_state = classify_project_index(&project_root).await?;
    match index_state {
        ProjectIndexState::OutOfDate { found, expected } => {
            emit_stale_schema_warning(&project_root, &*fmt, format, Some((found, expected)));
            return Err(anyhow::anyhow!(
                "index schema at {} is out of date (found v{found}, expected v{expected}); run `1up reindex`",
                config::project_db_path(&project_root).display()
            ));
        }
        ProjectIndexState::UnknownUnreadable => {
            emit_stale_schema_warning(&project_root, &*fmt, format, None);
            return Err(anyhow::anyhow!(
                "index at {} is unreadable; run `1up reindex`",
                config::project_db_path(&project_root).display()
            ));
        }
        ProjectIndexState::Current | ProjectIndexState::NotCreated => {}
    }
    let index_ready = matches!(index_state, ProjectIndexState::Current);

    if let Some(pid) = lifecycle::is_daemon_running()? {
        if index_ready {
            let already_registered = registry
                .projects
                .iter()
                .any(|p| p.project_root == project_root);

            registry.register(&project_id, &project_root, Some(indexing_config.clone()))?;
            lifecycle::send_sighup(pid)?;
            let msg = if already_registered {
                format!(
                    "{init_prefix}Daemon already running (pid={pid}); project settings refreshed."
                )
            } else {
                format!(
                    "{init_prefix}Project registered. Daemon (pid={pid}) notified to watch {}.",
                    project_root.display()
                )
            };
            if already_registered {
                eprintln!("{}", fmt.format_message(&msg));
            } else {
                println!("{}", fmt.format_message(&msg));
            }
            return Ok(());
        }

        let stats = run_initial_index(&project_root, &source_root, &indexing_config).await?;
        registry.register(&project_id, &project_root, Some(indexing_config))?;
        lifecycle::send_sighup(pid)?;
        let msg = format!(
            "{init_prefix}Indexed {} files ({} segments). Daemon already running (pid={pid}); notified to reload. Run: 1up status to watch progress.",
            stats.files_indexed, stats.segments_stored,
        );
        println!("{}", fmt.format_index_summary(&msg, &stats.progress));
        return Ok(());
    }

    let stats = run_initial_index(&project_root, &source_root, &indexing_config).await?;

    registry.register(&project_id, &project_root, Some(indexing_config))?;

    // Double-check: another `1up start` may have spawned a daemon while we were indexing.
    if let Some(pid) = lifecycle::is_daemon_running()? {
        lifecycle::send_sighup(pid)?;
        let msg = format!(
            "{init_prefix}Indexed {} files ({} segments). Daemon already running (pid={pid}); notified to reload. Run: 1up status to watch progress.",
            stats.files_indexed, stats.segments_stored,
        );
        println!("{}", fmt.format_index_summary(&msg, &stats.progress));
        return Ok(());
    }

    let binary = lifecycle::current_binary_path()?;
    let pid = lifecycle::spawn_daemon(&binary)?;

    let msg = format!(
        "{init_prefix}Indexed {} files ({} segments). Daemon started (pid={pid}). Run: 1up status to watch progress.",
        stats.files_indexed, stats.segments_stored,
    );
    println!("{}", fmt.format_index_summary(&msg, &stats.progress));
    Ok(())
}

/// Classify the state of a project's on-disk index without mutating it.
///
/// Opens the project DB read-only and asks `schema::ensure_current` whether
/// the schema matches the running binary. The existing error substrings
/// (`"out of date"`, `"is missing"`, etc.) are stable contracts from
/// `schema.rs` and are matched here so the caller can pivot to a concrete
/// user-facing recovery message instead of forwarding a raw migration error.
async fn classify_project_index(project_root: &Path) -> anyhow::Result<ProjectIndexState> {
    let db_path = config::project_db_path(project_root);
    if !db_path.exists() {
        return Ok(ProjectIndexState::NotCreated);
    }

    let db = match Db::open_ro(&db_path).await {
        Ok(db) => db,
        Err(_) => return Ok(ProjectIndexState::UnknownUnreadable),
    };
    let conn = match db.connect() {
        Ok(conn) => conn,
        Err(_) => return Ok(ProjectIndexState::UnknownUnreadable),
    };

    match schema::ensure_current(&conn).await {
        Ok(()) => Ok(ProjectIndexState::Current),
        Err(err) => Ok(classify_schema_error(&err.to_string())),
    }
}

/// Map a `schema::ensure_current` error message to the index state it
/// represents. `schema.rs` emits stable substrings for the shapes we care
/// about; anything else is treated as unreadable so the user still gets a
/// `1up reindex` recovery message instead of a raw error.
fn classify_schema_error(message: &str) -> ProjectIndexState {
    if message.contains("index is missing") {
        return ProjectIndexState::NotCreated;
    }
    if message.contains("out of date") {
        if let Some((found, expected)) = parse_schema_versions(message) {
            return ProjectIndexState::OutOfDate { found, expected };
        }
        return ProjectIndexState::OutOfDate {
            found: 0,
            expected: constants::SCHEMA_VERSION,
        };
    }
    ProjectIndexState::UnknownUnreadable
}

/// Extract the `(found, expected)` schema versions from an
/// `ensure_current` error message of the form
/// `"... (found v<N>, expected v<M>) ..."`.
fn parse_schema_versions(message: &str) -> Option<(u32, u32)> {
    let found_idx = message.find("found v")? + "found v".len();
    let found_rest = &message[found_idx..];
    let found_end = found_rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(found_rest.len());
    let found: u32 = found_rest[..found_end].parse().ok()?;

    let expected_idx = message.find("expected v")? + "expected v".len();
    let expected_rest = &message[expected_idx..];
    let expected_end = expected_rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(expected_rest.len());
    let expected: u32 = expected_rest[..expected_end].parse().ok()?;

    Some((found, expected))
}

/// Emit the user-facing stale-schema warning to stdout in the current
/// output format. Non-JSON formatters print the free-form warning through
/// `format_message` (REQ-033 D3); JSON emits the machine-readable
/// `schema_out_of_date` object called out in design §3.2.
fn emit_stale_schema_warning(
    project_root: &Path,
    fmt: &dyn Formatter,
    format: OutputFormat,
    versions: Option<(u32, u32)>,
) {
    let db_path = config::project_db_path(project_root);
    let expected = versions
        .map(|(_, e)| e)
        .unwrap_or(constants::SCHEMA_VERSION);
    let found = versions.map(|(f, _)| f).unwrap_or(0);

    if matches!(format, OutputFormat::Json) {
        let payload = serde_json::json!({
            "status": "schema_out_of_date",
            "found": found,
            "expected": expected,
            "action": "1up reindex",
            "path": db_path.display().to_string(),
        });
        println!("{payload}");
        return;
    }

    let msg = match versions {
        Some((found, expected)) => format!(
            "warning: index schema at {} is out of date (found v{found}, expected v{expected}).\nRun: 1up reindex",
            db_path.display()
        ),
        None => format!(
            "warning: index at {} is unreadable and needs a rebuild.\nRun: 1up reindex",
            db_path.display()
        ),
    };
    println!("{}", fmt.format_message(&msg));
}

async fn run_initial_index(
    project_root: &Path,
    source_root: &Path,
    indexing_config: &IndexingConfig,
) -> anyhow::Result<pipeline::PipelineStats> {
    let mut setup = SetupTimings::new(Instant::now());
    let db_path = config::project_db_path(project_root);

    let db_start = Instant::now();
    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect_tuned().await?;
    schema::prepare_for_write(&conn).await?;
    setup.db_prepare_ms = db_start.elapsed().as_millis();

    let model_start = Instant::now();
    let mut runtime = EmbeddingRuntime::default();
    let status = runtime
        .prepare_for_indexing(indexing_config.embed_threads)
        .await;
    setup.model_prepare_ms = model_start.elapsed().as_millis();
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => {}
        EmbeddingLoadStatus::Downloaded => {
            eprintln!("info: embedding model downloaded successfully");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            eprintln!("warning: embedding model download previously failed; indexing without embeddings (semantic search will be unavailable). Delete ~/.local/share/1up/models/all-MiniLM-L6-v2/.download_failed to retry");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
            eprintln!(
                "warning: embedding model download failed ({err}); indexing without embeddings (semantic search will be unavailable)"
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err)) => {
            eprintln!(
                "warning: embedding model failed to load ({err}); indexing without embeddings (semantic search will be unavailable)"
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            eprintln!(
                "warning: embedding model unavailable; indexing without embeddings (semantic search will be unavailable)"
            );
        }
    }

    let stats = pipeline::run_with_scope_setup_and_progress_root(
        &conn,
        source_root,
        runtime.current_embedder(),
        &crate::shared::types::RunScope::Full,
        indexing_config,
        None,
        true,
        Some(setup),
        None,
        Some(project_root),
    )
    .await?;

    Ok(stats)
}

fn install_fences(project_root: &Path, fmt: &dyn Formatter) {
    for filename in constants::FENCE_TARGET_FILES {
        let file_path = project_root.join(filename);
        let existing = std::fs::read_to_string(&file_path).ok();
        let (new_content, action) = reminder::apply_fence(existing.as_deref());

        match action {
            reminder::FenceAction::AlreadyCurrent => {}
            reminder::FenceAction::Created => {
                if let Err(e) = std::fs::write(&file_path, &new_content) {
                    eprintln!("warning: failed to create {filename}: {e}");
                    continue;
                }
                eprintln!(
                    "{}",
                    fmt.format_message(&format!(
                        "Created {filename} with 1up agent reminder (fence v{}).",
                        reminder::FENCE_VERSION
                    ))
                );
            }
            reminder::FenceAction::Updated { old_version } => {
                if let Err(e) = std::fs::write(&file_path, &new_content) {
                    eprintln!("warning: failed to update {filename}: {e}");
                    continue;
                }
                eprintln!(
                    "{}",
                    fmt.format_message(&format!(
                        "Updated 1up reminder in {filename} ({old_version} -> {}).",
                        reminder::FENCE_VERSION
                    ))
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_schema_error_maps_out_of_date_with_versions() {
        // Matches the substring shape `schema::ensure_current` emits for
        // `Some(v) if v < SCHEMA_VERSION`.
        let msg = "schema migration failed: index schema is out of date (found v4, expected v12); run `1up reindex`";
        assert_eq!(
            classify_schema_error(msg),
            ProjectIndexState::OutOfDate {
                found: 4,
                expected: 12,
            }
        );
    }

    #[test]
    fn classify_schema_error_maps_missing_index_to_not_created() {
        // Matches the `None` branch of `ensure_current` when no user tables exist.
        let msg = "schema migration failed: index is missing; run `1up reindex`";
        assert_eq!(classify_schema_error(msg), ProjectIndexState::NotCreated);
    }

    #[test]
    fn classify_schema_error_falls_back_to_unknown_unreadable() {
        // Newer-than-supported schema version is neither stale nor missing; we
        // still want the user pointed at `1up reindex` rather than a raw error.
        let msg = "schema migration failed: index schema v99 is newer than this binary supports";
        assert_eq!(
            classify_schema_error(msg),
            ProjectIndexState::UnknownUnreadable
        );
    }

    #[test]
    fn parse_schema_versions_reads_surrounding_parentheses() {
        assert_eq!(
            parse_schema_versions("... (found v4, expected v12) ..."),
            Some((4, 12))
        );
    }

    #[test]
    fn parse_schema_versions_returns_none_without_markers() {
        assert_eq!(parse_schema_versions("nothing to parse here"), None);
    }
}
