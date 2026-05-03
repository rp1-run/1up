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
use sha2::{Digest, Sha256};

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
use crate::shared::progress::{ProgressState, ProgressUi};
use crate::shared::project;
use crate::shared::types::{IndexingConfig, OutputFormat, SetupTimings};
use crate::storage::db::Db;
use crate::storage::schema;

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
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            "Embedding model unavailable".to_string()
        }
    }
}

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
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;

    let (project_id, initialized_now) = project::ensure_project_id(&project_root)?;
    if initialized_now {
        tracing::info!(
            "initialized project {} at {} during start",
            project_id,
            project_root.display()
        );
    }
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
    let index_ready = matches!(index_state, ProjectIndexState::Current);

    let daemon_state = lifecycle::probe_daemon()?;

    if index_ready {
        let already_registered = registry_contains_project(&registry, &project_root);
        registry.register_with_source(
            &project_id,
            &project_root,
            &source_root,
            Some(indexing_config),
        )?;
        let daemon = ensure_daemon_after_registration(daemon_state)?;
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

    let show_progress_ui = format == OutputFormat::Human;
    let stats = run_initial_index(
        &project_root,
        &source_root,
        &indexing_config,
        show_progress_ui,
    )
    .await?;
    registry.register_with_source(
        &project_id,
        &project_root,
        &source_root,
        Some(indexing_config),
    )?;
    let daemon = ensure_daemon_after_registration(lifecycle::probe_daemon()?)?;
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
    let lock_path = startup_lock_path(&xdg_root, project_root);
    let validated_path = validate_regular_file_path(&lock_path, &xdg_root)?;
    let mut file = open_startup_lock_file(&validated_path)?;
    let deadline = Instant::now() + STARTUP_GUARD_TIMEOUT;

    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(StartupGuardAcquire::Acquired(StartupGuard { _lock: lock })),
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
    xdg_root.join(format!("startup-{}.lock", startup_lock_key(project_root)))
}

fn startup_lock_key(project_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_root.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn registry_contains_project(registry: &Registry, project_root: &Path) -> bool {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    registry
        .projects
        .iter()
        .any(|project| project.project_root == canonical)
}

fn ensure_daemon_after_registration(
    initial_state: DaemonProbeState,
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
            let spawned_pid = lifecycle::spawn_daemon(&binary)?;
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
/// `format_message` (REQ-033 D3); JSON emits the machine-readable
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
    source_root: &Path,
    indexing_config: &IndexingConfig,
    show_progress_ui: bool,
) -> anyhow::Result<pipeline::PipelineStats> {
    let mut setup = SetupTimings::new(Instant::now());
    let db_path = config::project_db_path(project_root);
    let mut setup_spinner = spin("Preparing database", show_progress_ui);

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
        .await;
    setup.model_prepare_ms = model_start.elapsed().as_millis();
    let status_message = model_status_message(&status);
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => model_spinner.success(),
        EmbeddingLoadStatus::Downloaded => model_spinner.success_with(status_message),
        EmbeddingLoadStatus::Unavailable(_) => model_spinner.warn_with(status_message),
    }

    let stats = pipeline::run_with_scope_setup_and_progress_root(
        &conn,
        source_root,
        runtime.current_embedder(),
        &crate::shared::types::RunScope::Full,
        indexing_config,
        None,
        show_progress_ui,
        Some(setup),
        None,
        Some(project_root),
    )
    .await?;

    Ok(stats)
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
