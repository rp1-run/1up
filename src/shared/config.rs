use std::path::PathBuf;

use crate::shared::constants::{
    EMBED_THREADS_ENV_VAR, FORCE_ANN_SEARCH_ENV_VAR, GC_MIGRATION_PRUNE_ENV_VAR,
    INDEX_JOBS_ENV_VAR, INDEX_WRITE_BATCH_FILES_ENV_VAR, MODEL_ARTIFACT_MANIFEST_FILENAME,
    MODEL_CURRENT_MANIFEST_FILENAME, MODEL_STAGING_DIRNAME, MODEL_VERIFIED_DIRNAME,
    UPDATE_CHECK_CACHE_FILENAME,
};
use crate::shared::errors::{ConfigError, OneupError};
use crate::shared::types::IndexingConfig;

const APP_NAME: &str = "1up";

/// Returns the XDG config directory for 1up (~/.config/1up/).
#[allow(dead_code)]
pub fn config_dir() -> Result<PathBuf, OneupError> {
    let base = dirs::config_dir()
        .ok_or_else(|| ConfigError::XdgDirNotFound("XDG config directory not found".to_string()))?;
    Ok(base.join(APP_NAME))
}

/// Returns the XDG data directory for 1up (~/.local/share/1up/).
pub fn data_dir() -> Result<PathBuf, OneupError> {
    let base = dirs::data_dir()
        .ok_or_else(|| ConfigError::XdgDirNotFound("XDG data directory not found".to_string()))?;
    Ok(base.join(APP_NAME))
}

/// Returns the path to the embedding model directory.
pub fn model_dir() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join("models").join("all-MiniLM-L6-v2"))
}

/// Returns the path to the download failure marker file.
///
/// When present, indicates a previous model download failed and
/// the system should not re-attempt until the marker is cleared.
pub fn download_failure_marker() -> Result<PathBuf, OneupError> {
    Ok(model_dir()?.join(".download_failed"))
}

/// Returns the path to the directory containing verified model artifact sets.
pub fn model_verified_dir() -> Result<PathBuf, OneupError> {
    Ok(model_dir()?.join(MODEL_VERIFIED_DIRNAME))
}

/// Returns the path to the staging directory for model downloads/imports.
pub fn model_staging_dir() -> Result<PathBuf, OneupError> {
    Ok(model_dir()?.join(MODEL_STAGING_DIRNAME))
}

/// Returns the path to the active model artifact pointer file.
pub fn model_current_manifest_path() -> Result<PathBuf, OneupError> {
    Ok(model_dir()?.join(MODEL_CURRENT_MANIFEST_FILENAME))
}

/// Returns the path to a specific verified model artifact directory.
pub fn verified_model_artifact_dir(artifact_id: &str) -> Result<PathBuf, OneupError> {
    Ok(model_verified_dir()?.join(artifact_id))
}

/// Returns the path to the manifest for a specific verified model artifact.
pub fn verified_model_manifest_path(artifact_id: &str) -> Result<PathBuf, OneupError> {
    Ok(verified_model_artifact_dir(artifact_id)?.join(MODEL_ARTIFACT_MANIFEST_FILENAME))
}

/// Returns the path to the daemon PID file.
pub fn pid_file_path() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join("daemon.pid"))
}

/// Returns the path to the daemon search socket.
pub fn daemon_socket_path() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join("daemon.sock"))
}

/// Returns the path to the update-check cache file (~/.local/share/1up/update-check.json).
pub fn update_check_cache_path() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join(UPDATE_CHECK_CACHE_FILENAME))
}

/// Returns the path to the global project registry.
pub fn projects_registry_path() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join("projects.json"))
}

/// Returns the path to the project-local .1up directory for a given project root.
pub fn project_dot_dir(project_root: &std::path::Path) -> PathBuf {
    project_root.join(".1up")
}

/// Returns the path to the project-local database file.
pub fn project_db_path(project_root: &std::path::Path) -> PathBuf {
    project_dot_dir(project_root).join("index.db")
}

/// Filename prefix for build-aside staging index files (`index.db.rebuild-<uuid>`).
/// Single source of truth shared by [`project_staging_db_path`] (which builds the
/// path) and the storage-layer staging-leaf validation (which clamps it), so the
/// two cannot drift.
pub const STAGING_INDEX_DB_PREFIX: &str = "index.db.rebuild-";

/// Returns a fresh uuid-suffixed staging path for a non-destructive index
/// rebuild, e.g. `<project>/.1up/index.db.rebuild-<uuid>`.
///
/// The refreshed index is built into this sibling file under the same `.1up/`
/// directory and atomically renamed over `index.db` once finalized, so the live
/// index is never torn down in place. A fresh uuid per call keeps a staging file
/// left behind by a previously-aborted rebuild from colliding with a new one.
pub fn project_staging_db_path(state_root: &std::path::Path) -> PathBuf {
    project_dot_dir(state_root).join(format!("{STAGING_INDEX_DB_PREFIX}{}", uuid::Uuid::new_v4()))
}

/// Returns the path to the project-local daemon status file.
pub fn project_daemon_status_path(project_root: &std::path::Path) -> PathBuf {
    project_dot_dir(project_root).join("daemon_status.json")
}

/// Returns the path to the single-writer rebuild lockfile within the `.1up`
/// directory. Keyed on the state root so linked worktrees that share one
/// physical `.1up/` contend on the same lock.
pub fn project_rebuild_lock_path(state_root: &std::path::Path) -> PathBuf {
    project_dot_dir(state_root).join("rebuild.lock")
}

/// Returns the path to the project_id file within the .1up directory.
pub fn project_id_path(project_root: &std::path::Path) -> PathBuf {
    project_dot_dir(project_root).join("project_id")
}

pub fn resolve_indexing_config(
    cli_jobs: Option<usize>,
    cli_embed_threads: Option<usize>,
    persisted: Option<&IndexingConfig>,
) -> Result<IndexingConfig, OneupError> {
    resolve_indexing_config_with_globs(cli_jobs, cli_embed_threads, None, None, None, persisted)
}

/// Like [`resolve_indexing_config`], additionally resolving the per-project
/// include/exclude glob and dotfile-override fields. CLI-supplied values take
/// precedence over the persisted registry entry, which in turn wins over the
/// empty default (no env-var layer: these are list-shaped project config, not
/// scalar tuning knobs). Callers with no CLI-level glob surface (e.g. `1up
/// start`, MCP, the daemon worker) pass `None` and still inherit whatever was
/// persisted, so `ScanFilter` construction downstream stays consistent
/// regardless of entry point.
pub fn resolve_indexing_config_with_globs(
    cli_jobs: Option<usize>,
    cli_embed_threads: Option<usize>,
    cli_include_globs: Option<Vec<String>>,
    cli_exclude_globs: Option<Vec<String>>,
    cli_index_hidden_dirs: Option<Vec<String>>,
    persisted: Option<&IndexingConfig>,
) -> Result<IndexingConfig, OneupError> {
    let env_jobs = read_positive_env(INDEX_JOBS_ENV_VAR)?;
    let env_embed_threads = read_positive_env(EMBED_THREADS_ENV_VAR)?;
    let env_write_batch_files = read_positive_env(INDEX_WRITE_BATCH_FILES_ENV_VAR)?;

    IndexingConfig::from_sources_with_globs(
        cli_jobs
            .or(env_jobs)
            .or(persisted.map(|config| config.jobs)),
        cli_embed_threads
            .or(env_embed_threads)
            .or(persisted.map(|config| config.embed_threads)),
        env_write_batch_files.or(persisted.map(|config| config.write_batch_files)),
        cli_include_globs.or_else(|| persisted.map(|config| config.include_globs.clone())),
        cli_exclude_globs.or_else(|| persisted.map(|config| config.exclude_globs.clone())),
        cli_index_hidden_dirs.or_else(|| persisted.map(|config| config.index_hidden_dirs.clone())),
    )
    .map_err(|err| ConfigError::ReadFailed(err).into())
}

/// Reports whether automatic `SupersededSameSource` context pruning is enabled
/// at migration time, via [`GC_MIGRATION_PRUNE_ENV_VAR`]. Default OFF: unset,
/// empty, or `"0"` disable it; any other value enables it (mirrors
/// `indexer::embedder`'s `model_downloads_disabled` env-flag convention).
pub fn migration_gc_prune_enabled() -> bool {
    migration_gc_prune_enabled_value(std::env::var_os(GC_MIGRATION_PRUNE_ENV_VAR))
}

fn migration_gc_prune_enabled_value(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|raw| !raw.is_empty() && raw != "0")
}

/// Reports whether the approximate `vector_top_k` DiskANN search path is force
/// opted-in via [`FORCE_ANN_SEARCH_ENV_VAR`]. Default OFF: unset, empty, or
/// `"0"` keep the exact `vector_distance_cos` scan as the default for all corpus
/// sizes; any other value opts into the ANN path (mirrors
/// [`migration_gc_prune_enabled`]'s env-flag convention).
pub fn force_ann_search_enabled() -> bool {
    force_ann_search_enabled_value(std::env::var_os(FORCE_ANN_SEARCH_ENV_VAR))
}

fn force_ann_search_enabled_value(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|raw| !raw.is_empty() && raw != "0")
}

fn read_positive_env(name: &str) -> Result<Option<usize>, OneupError> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(None);
    };
    let value = raw.to_string_lossy();
    let parsed = value.parse::<usize>().map_err(|_| {
        ConfigError::ReadFailed(format!("{name} must be a positive integer, got {value}"))
    })?;

    if parsed == 0 {
        return Err(ConfigError::ReadFailed(format!(
            "{name} must be a positive integer, got {value}"
        ))
        .into());
    }

    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            Self {
                saved: keys
                    .iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn clear_indexing_env() {
        std::env::remove_var(INDEX_JOBS_ENV_VAR);
        std::env::remove_var(EMBED_THREADS_ENV_VAR);
        std::env::remove_var(INDEX_WRITE_BATCH_FILES_ENV_VAR);
    }

    #[test]
    fn resolve_indexing_config_prefers_cli_then_env_then_registry() {
        // Route onto the single process-wide `shared::fs::ENV_MUTEX` so this
        // env-mutating test serializes against every other one in the binary
        // (not just config.rs's own tests). Poison-tolerant acquire.
        let _lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::new(&[
            INDEX_JOBS_ENV_VAR,
            EMBED_THREADS_ENV_VAR,
            INDEX_WRITE_BATCH_FILES_ENV_VAR,
        ]);
        clear_indexing_env();

        std::env::set_var(INDEX_JOBS_ENV_VAR, "7");
        std::env::set_var(EMBED_THREADS_ENV_VAR, "6");
        std::env::set_var(INDEX_WRITE_BATCH_FILES_ENV_VAR, "5");

        let persisted = IndexingConfig::new(3, 2, 4).unwrap();
        let resolved = resolve_indexing_config(Some(9), Some(8), Some(&persisted)).unwrap();

        assert_eq!(resolved.jobs, 9);
        assert_eq!(resolved.embed_threads, 8);
        assert_eq!(resolved.write_batch_files, 5);
    }

    #[test]
    fn resolve_indexing_config_uses_registry_when_cli_and_env_missing() {
        let _lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::new(&[
            INDEX_JOBS_ENV_VAR,
            EMBED_THREADS_ENV_VAR,
            INDEX_WRITE_BATCH_FILES_ENV_VAR,
        ]);
        clear_indexing_env();

        let persisted = IndexingConfig::new(3, 2, 4).unwrap();
        let resolved = resolve_indexing_config(None, None, Some(&persisted)).unwrap();

        assert_eq!(resolved, persisted);
    }

    #[test]
    fn resolve_indexing_config_uses_conservative_defaults() {
        let _lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::new(&[
            INDEX_JOBS_ENV_VAR,
            EMBED_THREADS_ENV_VAR,
            INDEX_WRITE_BATCH_FILES_ENV_VAR,
        ]);
        clear_indexing_env();

        let resolved = resolve_indexing_config(None, None, None).unwrap();

        assert!(resolved.jobs >= 1);
        assert_eq!(
            resolved.embed_threads,
            IndexingConfig::default_embed_threads_for(resolved.jobs)
        );
        assert_eq!(
            resolved.write_batch_files,
            IndexingConfig::default_write_batch_files_for(resolved.jobs)
        );
    }

    #[test]
    fn resolve_indexing_config_keeps_single_file_registry_override() {
        let _lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::new(&[
            INDEX_JOBS_ENV_VAR,
            EMBED_THREADS_ENV_VAR,
            INDEX_WRITE_BATCH_FILES_ENV_VAR,
        ]);
        clear_indexing_env();

        let persisted = IndexingConfig::new(3, 2, 1).unwrap();
        let resolved = resolve_indexing_config(None, None, Some(&persisted)).unwrap();

        assert_eq!(resolved.write_batch_files, 1);
    }

    #[test]
    fn resolve_indexing_config_with_globs_prefers_cli_over_registry() {
        let _lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::new(&[
            INDEX_JOBS_ENV_VAR,
            EMBED_THREADS_ENV_VAR,
            INDEX_WRITE_BATCH_FILES_ENV_VAR,
        ]);
        clear_indexing_env();

        let persisted = IndexingConfig::with_glob_config(
            3,
            2,
            1,
            vec!["persisted-include/**".to_string()],
            vec!["persisted-exclude/**".to_string()],
            vec![".persisted-dir".to_string()],
        )
        .unwrap();

        let resolved = resolve_indexing_config_with_globs(
            None,
            None,
            Some(vec!["cli-include/**".to_string()]),
            Some(vec!["cli-exclude/**".to_string()]),
            Some(vec![".github/workflows".to_string()]),
            Some(&persisted),
        )
        .unwrap();

        assert_eq!(resolved.include_globs, vec!["cli-include/**".to_string()]);
        assert_eq!(resolved.exclude_globs, vec!["cli-exclude/**".to_string()]);
        assert_eq!(
            resolved.index_hidden_dirs,
            vec![".github/workflows".to_string()]
        );
    }

    #[test]
    fn resolve_indexing_config_with_globs_falls_back_to_registry_then_empty() {
        let _lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::new(&[
            INDEX_JOBS_ENV_VAR,
            EMBED_THREADS_ENV_VAR,
            INDEX_WRITE_BATCH_FILES_ENV_VAR,
        ]);
        clear_indexing_env();

        let persisted = IndexingConfig::with_glob_config(
            3,
            2,
            1,
            vec!["persisted-include/**".to_string()],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        let resolved =
            resolve_indexing_config_with_globs(None, None, None, None, None, Some(&persisted))
                .unwrap();
        assert_eq!(
            resolved.include_globs,
            vec!["persisted-include/**".to_string()]
        );
        assert!(resolved.exclude_globs.is_empty());
        assert!(resolved.index_hidden_dirs.is_empty());

        let no_registry =
            resolve_indexing_config_with_globs(None, None, None, None, None, None).unwrap();
        assert!(no_registry.include_globs.is_empty());
        assert!(no_registry.exclude_globs.is_empty());
        assert!(no_registry.index_hidden_dirs.is_empty());
    }

    #[test]
    fn resolve_indexing_config_rejects_invalid_env_values() {
        let _lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::new(&[
            INDEX_JOBS_ENV_VAR,
            EMBED_THREADS_ENV_VAR,
            INDEX_WRITE_BATCH_FILES_ENV_VAR,
        ]);
        clear_indexing_env();

        std::env::set_var(INDEX_JOBS_ENV_VAR, "0");

        let err = resolve_indexing_config(None, None, None).unwrap_err();
        assert!(err.to_string().contains(INDEX_JOBS_ENV_VAR));
    }

    #[test]
    fn migration_gc_prune_enabled_value_semantics() {
        use std::ffi::OsString;

        assert!(!migration_gc_prune_enabled_value(None));
        assert!(!migration_gc_prune_enabled_value(Some(OsString::from(""))));
        assert!(!migration_gc_prune_enabled_value(Some(OsString::from("0"))));
        assert!(migration_gc_prune_enabled_value(Some(OsString::from("1"))));
        assert!(migration_gc_prune_enabled_value(Some(OsString::from(
            "true"
        ))));
    }

    #[test]
    fn force_ann_search_enabled_value_semantics() {
        use std::ffi::OsString;

        assert!(!force_ann_search_enabled_value(None));
        assert!(!force_ann_search_enabled_value(Some(OsString::from(""))));
        assert!(!force_ann_search_enabled_value(Some(OsString::from("0"))));
        assert!(force_ann_search_enabled_value(Some(OsString::from("1"))));
        assert!(force_ann_search_enabled_value(Some(OsString::from("true"))));
    }
}
