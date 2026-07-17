use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
};

use clap::Args;
#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};

use crate::cli::output::{
    formatter_for, Formatter, ProjectListIndexStatus, StartResultInfo, StartStatus,
};
use crate::daemon::lifecycle;
use crate::daemon::lifecycle::DaemonProbeState;
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::constants;
#[cfg(unix)]
use crate::shared::fs::{ensure_secure_xdg_root, validate_regular_file_path};
#[cfg(unix)]
use crate::shared::lock_reap::flock_still_names_path;
use crate::shared::lock_reap::{lock_file_name, project_lock_key};
use crate::shared::progress::{ProgressState, ProgressUi};
use crate::shared::project;
use crate::shared::types::{IndexingConfig, OutputFormat, SetupTimings, WorktreeContext};
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments;

const STARTUP_GUARD_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_GUARD_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const DAEMON_OBSERVE_TIMEOUT: Duration = Duration::from_secs(2);
const DAEMON_OBSERVE_INTERVAL: Duration = Duration::from_millis(50);

fn unsupported_daemon_start_message() -> &'static str {
    "Background daemon workflows are not supported on this platform yet. No project was started. The retained project lifecycle is available on daemon-supported platforms through `1up start`, `1up status`, `1up list`, and `1up stop`."
}

fn spin(msg: impl Into<String>, show_progress_ui: bool) -> ProgressUi {
    ProgressUi::stderr_if(ProgressState::spinner(msg), show_progress_ui)
}

fn model_status_message(status: &EmbeddingLoadStatus) -> String {
    match status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => {
            "Embedding model ready".to_string()
        }
        EmbeddingLoadStatus::Downloaded => "Embedding model downloaded".to_string(),
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            "Embedding model unavailable (previous download failed)".to_string()
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
            format!("Model download failed ({err})")
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err)) => {
            format!("Embedding model failed to load ({err})")
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ArtifactsUnverifiable(
            err,
        )) => {
            format!("Embedding model artifacts failed verification ({err})")
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            "Embedding model unavailable".to_string()
        }
    }
}

/// Classification of an existing project's on-disk index state.
///
/// Produced by [`classify_project_index`] and consumed by `1up start` to
/// decide whether to (a) proceed with indexing, (b) warn the user that the
/// schema is stale and point at `1up reindex`, or
/// (c) warn that the index is unreadable and needs a reindex.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectIndexState {
    /// `.1up/index.db` does not exist yet. Indexing will create it.
    NotCreated,
    /// Schema matches the current `SCHEMA_VERSION`.
    Current,
    /// An older schema version is on disk.
    OutOfDate { found: u32, expected: u32 },
    /// The on-disk schema version is newer than this binary supports.
    /// The recovery action is to upgrade `1up`, not to reindex.
    NewerThanSupported { found: u32 },
    /// The database file exists but its schema could not be determined
    /// (missing schema metadata, corrupt file, or equivalent). The
    /// recovery action is `1up reindex`.
    UnknownUnreadable,
}

#[derive(Args)]
pub struct StartArgs {
    /// Project root to start (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Maximum concurrent parse workers; overrides ONEUP_INDEX_JOBS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub jobs: Option<usize>,

    /// ONNX intra-op threads; overrides ONEUP_EMBED_THREADS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub embed_threads: Option<usize>,

    /// Scope cone to index (required for monorepos over FILE_COUNT_THRESHOLD)
    #[arg(long, value_name = "PATH")]
    pub scope: Option<String>,

    /// Print stable plain text output for simple scripts
    #[arg(long, conflicts_with = "format")]
    pub plain: bool,

    /// Output format override (defaults to human)
    #[arg(long, short = 'f', hide = true, conflicts_with = "plain")]
    pub format: Option<OutputFormat>,
}

#[cfg(unix)]
struct StartupGuard {
    _lock: Flock<File>,
}

#[cfg(not(unix))]
struct StartupGuard;

enum StartupGuardAcquire {
    Acquired(StartupGuard),
    Busy(DaemonProbeState),
}

struct DaemonStartOutcome {
    status: StartStatus,
    pid: Option<u32>,
}

pub async fn exec(args: StartArgs, format: OutputFormat) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root_for_creation(
        std::path::Path::new(&args.path),
    )?;
    let project_root = resolved.state_root;
    let source_root = resolved.source_root;
    let worktree_context = resolved.worktree_context;
    let fmt = formatter_for(format);

    if !lifecycle::supports_daemon() {
        println!("{}", fmt.format_message(unsupported_daemon_start_message()));
        return Ok(());
    }

    let _startup_guard = match acquire_project_startup_guard(&project_root)? {
        StartupGuardAcquire::Acquired(guard) => guard,
        StartupGuardAcquire::Busy(probe) => {
            let result = startup_guard_busy_result(probe, &project_root, &source_root);
            emit_start_result(&*fmt, format, &result, false);
            return Ok(());
        }
    };

    let mut registry = Registry::load()?;
    let mut indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for_context(&worktree_context),
    )?;

    let (project_id, initialized_now) = project::ensure_project_id(&project_root)?;
    if initialized_now {
        tracing::info!(
            "initialized project {} at {} during start",
            project_id,
            project_root.display()
        );
    }
    // `.1up/.gitignore` (with `*`) is ensured inside `ensure_project_id`
    // above on every resolve, so `init`-first and already-initialized projects are
    // covered too — no separate gitignore step is needed here.
    let init_prefix = if initialized_now {
        format!("Initialized project {project_id}. ")
    } else {
        String::new()
    };

    // Classify the on-disk index before deciding the indexing branch. This
    // gives stale-schema users a concrete `1up reindex` message
    // instead of an opaque migration error bubbling up from the indexer.
    let index_state = classify_project_index(&project_root).await?;
    match index_state {
        ProjectIndexState::OutOfDate { found, expected } => {
            emit_stale_schema_warning(&project_root, &*fmt, format, found, expected);
            return Err(anyhow::anyhow!(
                "index schema at {} is out of date (found v{found}, expected v{expected}); run `1up reindex`",
                config::project_db_path(&project_root).display()
            ));
        }
        ProjectIndexState::NewerThanSupported { found } => {
            emit_binary_out_of_date_warning(&project_root, &*fmt, format, found);
            return Err(anyhow::anyhow!(
                "index schema at {} is v{found}, newer than this binary supports (v{expected}); run `1up update`",
                config::project_db_path(&project_root).display(),
                expected = constants::SCHEMA_VERSION,
            ));
        }
        ProjectIndexState::UnknownUnreadable => {
            emit_index_unreadable_warning(&project_root, &*fmt, format);
            return Err(anyhow::anyhow!(
                "index at {} is unreadable; run `1up reindex`",
                config::project_db_path(&project_root).display()
            ));
        }
        ProjectIndexState::Current | ProjectIndexState::NotCreated => {}
    }
    // A schema-`Current` index only counts as ready-to-serve when it
    // actually holds indexed content for this context. An index created while the
    // repo (or this worktree) had no indexable files is schema-current but empty.
    // Treating such an empty index as ready would let the early return below
    // permanently bypass the monorepo file-count gate once the repo grows past the
    // threshold — the empty-index gate bypass.
    let schema_current = matches!(index_state, ProjectIndexState::Current);
    let has_content = schema_current
        && index_has_content_for_context(&project_root, &worktree_context.context_id).await;
    let scope_provided = args.scope.is_some();

    // Evaluate the monorepo file-count gate ONLY when there is no servable content
    // AND no scope was given — the populated fast path must not pay for a file-count
    // scan, and an explicit `--scope` bypasses the gate by design. A schema-current
    // but empty index on a small unscoped repo (gate does not fire) is still treated
    // as ready so the daemon indexes it in the background, preserving the
    // existing-index skip contract; only an over-threshold, unscoped, empty (or
    // absent) index falls through to the facts envelope below, closing the
    // empty-index gate bypass.
    let gate_fires = if has_content || scope_provided {
        false
    } else {
        let gate_readiness =
            crate::mcp::ops::classify_readiness(&project_root, &source_root, &worktree_context)
                .await;
        crate::mcp::ops::should_return_facts_envelope(&project_root, &source_root, &gate_readiness)
            .await
            .unwrap_or(false)
    };

    // Ready-to-serve (skip the foreground initial index) when the index holds content
    // for this context, OR it is schema-current but small enough that the gate does
    // not fire (the daemon will index it in the background after registration).
    //
    // A `--scope` on an EMPTY (or absent) index must NOT be treated as ready
    // — the ready branch returns before scope application/indexing (line ~315), so the
    // daemon would then index the FULL repo, silently discarding the scope (the exact
    // full-repo accident the scope is meant to prevent). Force the scoped foreground
    // index whenever a scope is provided and there is no servable content yet.
    let index_ready = has_content || (schema_current && !gate_fires && !scope_provided);

    let daemon_state = lifecycle::probe_daemon()?;

    if index_ready {
        let already_registered = registry_contains_context(&registry, &worktree_context);
        registry.register_with_context(&project_id, &worktree_context, Some(indexing_config))?;
        let daemon = ensure_daemon_after_registration(daemon_state, &source_root)?;
        let msg =
            current_index_start_message(&init_prefix, &project_root, already_registered, &daemon);
        let result = StartResultInfo {
            status: daemon.status,
            project_id: Some(project_id),
            project_root: Some(project_root),
            source_root: Some(source_root),
            registered: Some(true),
            index_status: Some(ProjectListIndexStatus::Ready),
            pid: daemon.pid,
            message: msg,
            progress: None,
        };
        emit_start_result(
            &*fmt,
            format,
            &result,
            already_registered && matches!(format, OutputFormat::Human),
        );
        return Ok(());
    }

    // Not ready-to-serve. If the gate fired (over threshold, unscoped, no content),
    // emit the facts envelope instead of indexing. Only reachable when the
    // index is absent or empty AND the gate fired.
    if gate_fires {
        // Compute launch_subdir for facts envelope suggestions (same logic as
        // mcp/ops::resolve_project).
        let canonical_launch_dir = match std::path::Path::new(&args.path).canonicalize() {
            Ok(p) => p,
            Err(_) => std::path::Path::new(&args.path).to_path_buf(),
        };
        let launch_subdir = if canonical_launch_dir != source_root
            && canonical_launch_dir.starts_with(&source_root)
        {
            Some(canonical_launch_dir)
        } else {
            None
        };

        // Gate fires: return facts envelope instead of indexing
        match crate::mcp::ops::generate_facts_envelope(&source_root, launch_subdir).await {
            Ok(facts) => {
                // Output facts envelope in appropriate format
                match format {
                    OutputFormat::Json => {
                        if let Ok(payload) = serde_json::to_value(&facts) {
                            println!("{}", payload);
                        }
                    }
                    _ => {
                        // Human-readable facts envelope output
                        emit_facts_envelope(&*fmt, &facts);
                    }
                }
                // Exit with error code 1 to signal user must provide --scope
                return Err(anyhow::anyhow!(
                    "repository is over file count threshold; use --scope to proceed"
                ));
            }
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to generate facts envelope: {}",
                    err
                ));
            }
        }
    }

    // Gate allowed or scope provided: apply scope if given and proceed with indexing
    if let Some(scope_path) = args.scope {
        // Validate the requested scope through the shared validator so an
        // absolute path, a `../` escape, or an empty scope is refused with a clear
        // error instead of silently producing a zero-file, misleadingly "ready"
        // index. Use the canonicalized root (trailing slash trimmed) it returns.
        let scope_roots = crate::shared::types::ScopeRoots::new(vec![scope_path.clone()])
            .map_err(|err| anyhow::anyhow!("invalid --scope `{scope_path}`: {err}"))?;
        let canonical_scope = scope_roots
            .roots()
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("--scope must not be empty"))?;
        indexing_config.scope_roots = vec![canonical_scope.clone()];
        indexing_config.scope_globs = vec![format!("{}/**", canonical_scope)];
    }

    let show_progress_ui = format == OutputFormat::Human;
    let stats = run_initial_index(
        &project_root,
        &worktree_context,
        &indexing_config,
        show_progress_ui,
    )
    .await?;
    registry.register_with_context(&project_id, &worktree_context, Some(indexing_config))?;
    let daemon = ensure_daemon_after_registration(lifecycle::probe_daemon()?, &source_root)?;
    let msg = indexed_start_message(
        &init_prefix,
        stats.files_indexed,
        stats.segments_stored,
        &daemon,
    );
    let status = if matches!(daemon.status, StartStatus::StartupInProgress) {
        StartStatus::StartupInProgress
    } else {
        StartStatus::IndexedAndStarted
    };
    let result = StartResultInfo {
        status,
        project_id: Some(project_id),
        project_root: Some(project_root),
        source_root: Some(source_root),
        registered: Some(true),
        index_status: Some(ProjectListIndexStatus::Ready),
        pid: daemon.pid,
        message: msg,
        progress: Some(stats.progress),
    };
    emit_start_result(&*fmt, format, &result, false);
    Ok(())
}

#[cfg(unix)]
fn acquire_project_startup_guard(project_root: &Path) -> anyhow::Result<StartupGuardAcquire> {
    let xdg_root = ensure_secure_xdg_root()?;
    // Opportunistic, best-effort sweep of abandoned per-project lock files. Like
    // the MCP path, this is a natural integration point: the XDG root is already
    // resolved and about to gain another lock file, and `1up start`/daemon start
    // is a process boundary where a bounded sweep is acceptable. Never errors,
    // never meaningfully delays startup.
    crate::shared::lock_reap::reap_stale_locks(&xdg_root);
    acquire_project_startup_guard_in(&xdg_root, project_root)
}

/// Acquisition core, split from [`acquire_project_startup_guard`] so tests can
/// drive it against an isolated root instead of the real XDG data dir.
///
/// A successful flock is not sufficient on its own: a concurrent stale-lock
/// reaper may unlink the guard file between our open and our flock, leaving us
/// holding an orphaned inode that excludes nobody — a concurrent `1up start`
/// could then create and lock a fresh file at the same pathname. After every
/// successful flock we therefore verify the pathname still names the locked
/// inode and, if not, drop the orphan, reopen, and retry, bounded by
/// [`constants::LOCK_ACQUIRE_IDENTITY_RETRIES`] (independent of the busy-wait
/// deadline, which keeps governing the contended case).
#[cfg(unix)]
fn acquire_project_startup_guard_in(
    xdg_root: &Path,
    project_root: &Path,
) -> anyhow::Result<StartupGuardAcquire> {
    let lock_path = startup_lock_path(xdg_root, project_root);
    let validated_path = validate_regular_file_path(&lock_path, xdg_root)?;
    let mut file = open_startup_lock_file(&validated_path)?;
    let deadline = Instant::now() + STARTUP_GUARD_TIMEOUT;
    let mut identity_retries = 0usize;

    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => {
                if flock_still_names_path(&lock, &validated_path) {
                    return Ok(StartupGuardAcquire::Acquired(StartupGuard { _lock: lock }));
                }
                // Reaped/replaced between our open and flock; the descriptor
                // is orphaned and excludes nobody. Drop it and re-acquire.
                identity_retries += 1;
                if identity_retries > constants::LOCK_ACQUIRE_IDENTITY_RETRIES {
                    return Err(anyhow::anyhow!(
                        "startup guard {} kept being replaced during acquisition",
                        validated_path.display()
                    ));
                }
                drop(lock);
                file = open_startup_lock_file(&validated_path)?;
            }
            Err((returned_file, Errno::EWOULDBLOCK)) => {
                let probe = lifecycle::probe_daemon()?;
                if matches!(probe, DaemonProbeState::Running(_)) || Instant::now() >= deadline {
                    return Ok(StartupGuardAcquire::Busy(probe));
                }
                file = returned_file;
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(STARTUP_GUARD_RETRY_INTERVAL.min(remaining));
            }
            Err((_, errno)) => {
                return Err(anyhow::anyhow!(
                    "failed to lock startup guard {}: {errno}",
                    validated_path.display()
                ));
            }
        }
    }
}

#[cfg(not(unix))]
fn acquire_project_startup_guard(_project_root: &Path) -> anyhow::Result<StartupGuardAcquire> {
    Ok(StartupGuardAcquire::Acquired(StartupGuard))
}

#[cfg(unix)]
fn open_startup_lock_file(path: &Path) -> anyhow::Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(constants::SECURE_STATE_FILE_MODE)
        .open(path)?)
}

fn startup_lock_path(xdg_root: &Path, project_root: &Path) -> PathBuf {
    xdg_root.join(lock_file_name(
        constants::STARTUP_LOCK_PREFIX,
        &project_lock_key(project_root),
    ))
}

fn registry_contains_context(
    registry: &Registry,
    context: &crate::shared::types::WorktreeContext,
) -> bool {
    registry.contains_context(context)
}

fn ensure_daemon_after_registration(
    initial_state: DaemonProbeState,
    source_root: &std::path::Path,
) -> anyhow::Result<DaemonStartOutcome> {
    match initial_state {
        DaemonProbeState::Running(pid) => {
            lifecycle::send_sighup(pid)?;
            Ok(DaemonStartOutcome {
                status: StartStatus::AlreadyRunning,
                pid: Some(pid),
            })
        }
        DaemonProbeState::Starting => observe_existing_daemon_startup(),
        DaemonProbeState::NotRunning => {
            let binary = lifecycle::current_binary_path()?;
            let spawned_pid = lifecycle::spawn_daemon(&binary, source_root)?;
            observe_spawned_daemon(spawned_pid)
        }
    }
}

fn observe_existing_daemon_startup() -> anyhow::Result<DaemonStartOutcome> {
    match wait_for_daemon_ready()? {
        DaemonProbeState::Running(pid) => {
            lifecycle::send_sighup(pid)?;
            Ok(DaemonStartOutcome {
                status: StartStatus::AlreadyRunning,
                pid: Some(pid),
            })
        }
        DaemonProbeState::Starting | DaemonProbeState::NotRunning => Ok(DaemonStartOutcome {
            status: StartStatus::StartupInProgress,
            pid: None,
        }),
    }
}

fn observe_spawned_daemon(spawned_pid: u32) -> anyhow::Result<DaemonStartOutcome> {
    match wait_for_daemon_ready()? {
        DaemonProbeState::Running(pid) => {
            if pid != spawned_pid {
                lifecycle::send_sighup(pid)?;
            }
            Ok(DaemonStartOutcome {
                status: StartStatus::Started,
                pid: Some(pid),
            })
        }
        DaemonProbeState::Starting | DaemonProbeState::NotRunning => Ok(DaemonStartOutcome {
            status: StartStatus::StartupInProgress,
            pid: None,
        }),
    }
}

fn wait_for_daemon_ready() -> anyhow::Result<DaemonProbeState> {
    let deadline = Instant::now() + DAEMON_OBSERVE_TIMEOUT;
    let mut last_state = lifecycle::probe_daemon()?;

    loop {
        if matches!(last_state, DaemonProbeState::Running(_)) || Instant::now() >= deadline {
            return Ok(last_state);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(DAEMON_OBSERVE_INTERVAL.min(remaining));
        last_state = lifecycle::probe_daemon()?;
    }
}

fn startup_guard_busy_result(
    probe: DaemonProbeState,
    project_root: &Path,
    source_root: &Path,
) -> StartResultInfo {
    let project_id = project::read_project_id(project_root).ok();
    match probe {
        DaemonProbeState::Running(pid) => StartResultInfo {
            status: StartStatus::AlreadyRunning,
            project_id: project_id.clone(),
            project_root: Some(project_root.to_path_buf()),
            source_root: Some(source_root.to_path_buf()),
            registered: None,
            index_status: None,
            pid: Some(pid),
            message: format!(
                "Daemon already running (pid={pid}); another startup is refreshing project settings."
            ),
            progress: None,
        },
        DaemonProbeState::NotRunning | DaemonProbeState::Starting => StartResultInfo {
            status: StartStatus::StartupInProgress,
            project_id,
            project_root: Some(project_root.to_path_buf()),
            source_root: Some(source_root.to_path_buf()),
            registered: None,
            index_status: None,
            pid: None,
            message: "Daemon startup already in progress.".to_string(),
            progress: None,
        },
    }
}

fn current_index_start_message(
    init_prefix: &str,
    project_root: &Path,
    already_registered: bool,
    daemon: &DaemonStartOutcome,
) -> String {
    match daemon.status {
        StartStatus::AlreadyRunning => match daemon.pid {
            Some(pid) if already_registered => {
                format!("{init_prefix}Daemon already running (pid={pid}); project settings refreshed.")
            }
            Some(pid) => format!(
                "{init_prefix}Project registered. Daemon (pid={pid}) notified to watch {}.",
                project_root.display()
            ),
            None => format!("{init_prefix}Daemon already running; project settings refreshed."),
        },
        StartStatus::Started => match daemon.pid {
            Some(pid) => format!(
                "{init_prefix}Project registered. Daemon started (pid={pid}). Run: 1up status to watch progress."
            ),
            None => format!(
                "{init_prefix}Project registered. Daemon startup in progress. Run: 1up status to watch progress."
            ),
        },
        StartStatus::StartupInProgress => match daemon.pid {
            Some(pid) => format!(
                "{init_prefix}Project registered. Daemon startup in progress (pid={pid}). Run: 1up status to watch progress."
            ),
            None => format!(
                "{init_prefix}Project registered. Daemon startup in progress. Run: 1up status to watch progress."
            ),
        },
        StartStatus::IndexedAndStarted => unreachable!("current-index start does not index"),
    }
}

fn indexed_start_message(
    init_prefix: &str,
    files_indexed: usize,
    segments_stored: usize,
    daemon: &DaemonStartOutcome,
) -> String {
    match daemon.status {
        StartStatus::AlreadyRunning => match daemon.pid {
            Some(pid) => format!(
                "{init_prefix}Indexed {files_indexed} files ({segments_stored} segments). Daemon already running (pid={pid}); notified to reload. Run: 1up status to watch progress."
            ),
            None => format!(
                "{init_prefix}Indexed {files_indexed} files ({segments_stored} segments). Daemon already running; notified to reload. Run: 1up status to watch progress."
            ),
        },
        StartStatus::Started => match daemon.pid {
            Some(pid) => format!(
                "{init_prefix}Indexed {files_indexed} files ({segments_stored} segments). Daemon started (pid={pid}). Run: 1up status to watch progress."
            ),
            None => format!(
                "{init_prefix}Indexed {files_indexed} files ({segments_stored} segments). Daemon startup in progress. Run: 1up status to watch progress."
            ),
        },
        StartStatus::StartupInProgress => match daemon.pid {
            Some(pid) => format!(
                "{init_prefix}Indexed {files_indexed} files ({segments_stored} segments). Daemon startup in progress (pid={pid}). Run: 1up status to watch progress."
            ),
            None => format!(
                "{init_prefix}Indexed {files_indexed} files ({segments_stored} segments). Daemon startup in progress. Run: 1up status to watch progress."
            ),
        },
        StartStatus::IndexedAndStarted => unreachable!("daemon outcome is not an indexed status"),
    }
}

fn emit_start_result(
    fmt: &dyn Formatter,
    _format: OutputFormat,
    result: &StartResultInfo,
    stderr: bool,
) {
    let rendered = fmt.format_start_result(result);
    if stderr {
        eprintln!("{rendered}");
    } else {
        println!("{rendered}");
    }
}

/// Emit a facts envelope in human-readable format to stdout.
///
/// The envelope includes:
/// - File count statistics
/// - Vector estimate with bounds
/// - Ranked scope suggestions
/// - Workspace manifest detection
fn emit_facts_envelope(fmt: &dyn Formatter, envelope: &crate::mcp::types::FactsEnvelope) {
    let mut lines = vec![];

    lines.push(format!(
        "Repository has {} files (over {} threshold). Indexing requires a scope.",
        envelope.file_count_total,
        constants::FILE_COUNT_THRESHOLD
    ));
    lines.push(String::new());

    lines.push("Per-directory statistics:".to_string());
    for stat in &envelope.per_directory_stats {
        lines.push(format!(
            "  {}: {} files (~{} vectors)",
            stat.directory, stat.file_count, stat.estimated_vectors
        ));
    }
    lines.push(String::new());

    lines.push(format!(
        "Global estimate: {} vectors (range: {}-{})",
        envelope.vector_estimate_total,
        envelope
            .vector_estimate_low
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        envelope
            .vector_estimate_high
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    lines.push(String::new());

    if !envelope.suggestions.is_empty() {
        lines.push("To index, choose a scope:".to_string());
        lines.push("  1up start . --scope <directory>".to_string());
        lines.push("Examples:".to_string());
        for suggestion in &envelope.suggestions {
            lines.push(format!("  1up start . --scope {}", suggestion));
        }
    }

    let message = lines.join("\n");
    println!("{}", fmt.format_message(&message));
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

    // DB open/connect errors here are genuine I/O or libSQL faults, not
    // schema issues; propagate so the user sees the real cause instead of
    // being sent through an incorrect "reindex to recover" path.
    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;

    match schema::ensure_current(&conn, &schema::SchemaContext::new(&db_path, project_root)).await {
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
    if message.contains("newer than this binary supports") {
        // `schema.rs` emits "index schema v{N} is newer than this binary
        // supports (expected v{M}); ...". Recover the found version so we
        // can surface it; fall back to 0 if parsing fails.
        let found = parse_single_version(message, "index schema v").unwrap_or(0);
        return ProjectIndexState::NewerThanSupported { found };
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

/// Extract the integer following a `v` prefix after the given marker.
/// E.g. `parse_single_version("index schema v9 is newer ...", "index schema v")` -> `Some(9)`.
fn parse_single_version(message: &str, prefix: &str) -> Option<u32> {
    let idx = message.find(prefix)? + prefix.len();
    let rest = &message[idx..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
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
/// `format_message`; JSON emits the machine-readable
/// `schema_out_of_date` object called out in design §3.2.
fn emit_stale_schema_warning(
    project_root: &Path,
    fmt: &dyn Formatter,
    format: OutputFormat,
    found: u32,
    expected: u32,
) {
    let db_path = config::project_db_path(project_root);

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

    let msg = format!(
        "warning: index schema at {} is out of date (found v{found}, expected v{expected}).\nRun: 1up reindex",
        db_path.display()
    );
    println!("{}", fmt.format_message(&msg));
}

/// Emit the user-facing warning when the on-disk schema is newer than the
/// running binary supports. Recovery is `1up update` (upgrade the CLI), not
/// `1up reindex` -- reindexing with an older binary would immediately land
/// back in the same state.
fn emit_binary_out_of_date_warning(
    project_root: &Path,
    fmt: &dyn Formatter,
    format: OutputFormat,
    found: u32,
) {
    let db_path = config::project_db_path(project_root);
    let expected = constants::SCHEMA_VERSION;

    if matches!(format, OutputFormat::Json) {
        let payload = serde_json::json!({
            "status": "binary_out_of_date",
            "found": found,
            "expected": expected,
            "action": "1up update",
            "path": db_path.display().to_string(),
        });
        println!("{payload}");
        return;
    }

    let msg = format!(
        "warning: index schema at {} is v{found}, newer than this binary supports (v{expected}).\nRun: 1up update to upgrade the CLI.",
        db_path.display()
    );
    println!("{}", fmt.format_message(&msg));
}

/// Emit the user-facing warning when the index DB exists but its schema
/// metadata could not be interpreted. The recovery action is a reindex;
/// the envelope is distinct from the stale-schema envelope so downstream
/// tooling can tell the two apart.
fn emit_index_unreadable_warning(project_root: &Path, fmt: &dyn Formatter, format: OutputFormat) {
    let db_path = config::project_db_path(project_root);

    if matches!(format, OutputFormat::Json) {
        let payload = serde_json::json!({
            "status": "index_unreadable",
            "action": "1up reindex",
            "path": db_path.display().to_string(),
        });
        println!("{payload}");
        return;
    }

    let msg = format!(
        "warning: index at {} is unreadable and needs a rebuild.\nRun: 1up reindex",
        db_path.display()
    );
    println!("{}", fmt.format_message(&msg));
}

async fn run_initial_index(
    project_root: &Path,
    context: &WorktreeContext,
    indexing_config: &IndexingConfig,
    show_progress_ui: bool,
) -> anyhow::Result<pipeline::PipelineStats> {
    let mut setup = SetupTimings::new(Instant::now());
    let db_path = config::project_db_path(project_root);
    let mut setup_spinner = spin("Preparing database", show_progress_ui);

    // Single-writer rebuild lock: hold it across schema prepare + the pipeline
    // write so a concurrent rebuild of the shared index cannot race the initial
    // index. Released when this function returns (RAII).
    let _rebuild_lock = lifecycle::acquire_rebuild_lock(project_root)?;

    let db_start = Instant::now();
    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect_tuned().await?;
    schema::prepare_for_write(&conn).await?;
    setup.db_prepare_ms = db_start.elapsed().as_millis();
    setup_spinner.success();

    let mut model_spinner = spin("Loading embedding model", show_progress_ui);

    let model_start = Instant::now();
    let mut runtime = EmbeddingRuntime::default();
    let status = runtime
        .prepare_for_indexing_with_progress(indexing_config.embed_threads, show_progress_ui)
        .await?;
    setup.model_prepare_ms = model_start.elapsed().as_millis();
    let status_message = model_status_message(&status);
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => model_spinner.success(),
        EmbeddingLoadStatus::Downloaded => model_spinner.success_with(status_message),
        EmbeddingLoadStatus::Unavailable(_) => model_spinner.warn_with(status_message),
    }

    // One-shot CLI start indexing: not subject to the daemon's SIGTERM drain, so
    // it runs under a fresh token that is never cancelled.
    let stats = pipeline::run_with_context_scope_setup_and_progress_root(
        &conn,
        context,
        runtime.current_embedder(),
        &crate::shared::types::RunScope::Full,
        indexing_config,
        None,
        show_progress_ui,
        Some(setup),
        None,
        Some(project_root),
        &tokio_util::sync::CancellationToken::new(),
    )
    .await?;

    Ok(stats)
}

/// Whether the current index actually holds indexed content for `context_id`.
///
/// A schema-`Current` index can still be empty — e.g. it was created by an earlier
/// `start`/`init` while the repo (or this worktree) had no indexable files. Such an
/// index must NOT be treated as a completed build, or the monorepo file-count gate
/// is permanently bypassed once the repo grows. Mirrors the zero-segment
/// => not-built classification used by `1up list` and the MCP readiness path.
/// Best-effort: any DB open/query error is treated as "no content" so the safe
/// path (gate + initial index) is taken.
async fn index_has_content_for_context(project_root: &Path, context_id: &str) -> bool {
    let db_path = config::project_db_path(project_root);
    let Ok(db) = Db::open_ro(&db_path).await else {
        return false;
    };
    let Ok(conn) = db.connect() else {
        return false;
    };
    segments::count_segments_for_context(&conn, context_id)
        .await
        .map(|count| count > 0)
        .unwrap_or(false)
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
    fn classify_schema_error_maps_newer_than_supported_to_binary_out_of_date() {
        // `schema.rs` emits this exact substring when the on-disk schema is
        // newer than the running binary. The recovery action is `1up update`,
        // not `1up reindex` -- reindexing with an older binary would land
        // right back in the same state.
        let msg = "schema migration failed: index schema v99 is newer than this binary supports (expected v12); rebuild with a compatible binary or upgrade `1up`";
        assert_eq!(
            classify_schema_error(msg),
            ProjectIndexState::NewerThanSupported { found: 99 }
        );
    }

    #[test]
    fn classify_schema_error_falls_back_to_unknown_unreadable_for_unknown_shape() {
        // Any error shape we don't recognize falls back to the generic
        // unreadable state so the user still gets a `1up reindex` action.
        let msg = "schema migration failed: some unexpected libsql error";
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

    #[test]
    fn parse_single_version_extracts_digits_after_prefix() {
        assert_eq!(
            parse_single_version("index schema v42 is newer", "index schema v"),
            Some(42)
        );
    }

    #[test]
    fn parse_single_version_returns_none_without_prefix() {
        assert_eq!(
            parse_single_version("no marker here", "index schema v"),
            None
        );
    }

    #[test]
    fn unsupported_daemon_start_message_mentions_only_retained_lifecycle_commands() {
        let message = unsupported_daemon_start_message();

        for retained in ["1up start", "1up status", "1up list", "1up stop"] {
            assert!(
                message.contains(retained),
                "unsupported daemon guidance should mention retained command {retained}; message={message}"
            );
        }

        for hidden in ["1up init", "1up index", "1up reindex", "1up add-mcp"] {
            assert!(
                !message.contains(hidden),
                "unsupported daemon guidance must not mention hidden command {hidden}; message={message}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;

    /// Canonicalize the tempdir (macOS `/var` -> `/private/var`) so secure-fs
    /// path validation sees a symlink-free root, matching production
    /// `ensure_secure_xdg_root` guarantees.
    fn root_of(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().unwrap()
    }

    #[test]
    fn startup_lock_name_matches_reaper_namespace() {
        let name_path = startup_lock_path(Path::new("/xdg"), Path::new("/some/project"));
        let name = name_path.file_name().unwrap().to_str().unwrap();
        assert!(
            crate::shared::lock_reap::is_reapable_name(name),
            "minted guard name {name:?} must parse under the reaper's strict namespace"
        );
    }

    #[test]
    fn startup_guard_acquires_in_isolated_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let project = Path::new("/some/project");

        let acquired = acquire_project_startup_guard_in(&root, project).unwrap();
        assert!(
            matches!(acquired, StartupGuardAcquire::Acquired(_)),
            "uncontended startup guard must be acquired"
        );
    }

    #[test]
    fn startup_guard_survives_prior_unlinked_holder_descriptor() {
        // A descriptor orphaned by the reaper (open + unlinked pathname) must
        // not exclude a fresh acquirer: acquisition recreates the path and
        // verifies its own descriptor still names it.
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let project = Path::new("/some/project");
        let lock_path = startup_lock_path(&root, project);

        let orphan = File::create(&lock_path).unwrap();
        let orphan_lock = Flock::lock(orphan, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, errno)| errno)
            .unwrap();
        std::fs::remove_file(&lock_path).unwrap();

        let acquired = acquire_project_startup_guard_in(&root, project)
            .expect("an orphaned (unlinked) holder must not block acquisition");
        assert!(matches!(acquired, StartupGuardAcquire::Acquired(_)));
        drop(orphan_lock);
    }
}
