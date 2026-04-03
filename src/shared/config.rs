use std::path::PathBuf;

use crate::shared::constants::{
    EMBED_THREADS_ENV_VAR, INDEX_JOBS_ENV_VAR, INDEX_WRITE_BATCH_FILES_ENV_VAR,
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

/// Returns the path to the daemon PID file.
pub fn pid_file_path() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join("daemon.pid"))
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

/// Returns the path to the project_id file within the .1up directory.
pub fn project_id_path(project_root: &std::path::Path) -> PathBuf {
    project_dot_dir(project_root).join("project_id")
}

pub fn resolve_indexing_config(
    cli_jobs: Option<usize>,
    cli_embed_threads: Option<usize>,
    persisted: Option<&IndexingConfig>,
) -> Result<IndexingConfig, OneupError> {
    let env_jobs = read_positive_env(INDEX_JOBS_ENV_VAR)?;
    let env_embed_threads = read_positive_env(EMBED_THREADS_ENV_VAR)?;
    let env_write_batch_files = read_positive_env(INDEX_WRITE_BATCH_FILES_ENV_VAR)?;

    IndexingConfig::from_sources(
        cli_jobs
            .or(env_jobs)
            .or(persisted.map(|config| config.jobs)),
        cli_embed_threads
            .or(env_embed_threads)
            .or(persisted.map(|config| config.embed_threads)),
        env_write_batch_files.or(persisted.map(|config| config.write_batch_files)),
    )
    .map_err(|err| ConfigError::ReadFailed(err).into())
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
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

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
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
        assert_eq!(resolved.write_batch_files, 1);
    }

    #[test]
    fn resolve_indexing_config_rejects_invalid_env_values() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
}
