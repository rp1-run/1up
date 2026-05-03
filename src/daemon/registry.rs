#[cfg(unix)]
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};

use crate::shared::config;
use crate::shared::constants::{SECURE_STATE_FILE_MODE, XDG_STATE_DIR_MODE};
use crate::shared::errors::{DaemonError, OneupError};
use crate::shared::fs::{atomic_replace, ensure_secure_xdg_root, validate_regular_file_path};
use crate::shared::project::canonical_project_root;
use crate::shared::types::IndexingConfig;

const REGISTRY_LOCK_FILE: &str = "projects.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub project_id: String,
    pub project_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root: Option<PathBuf>,
    pub registered_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<IndexingConfig>,
}

impl ProjectEntry {
    pub fn source_root(&self) -> &Path {
        self.source_root.as_deref().unwrap_or(&self.project_root)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Registry {
    pub projects: Vec<ProjectEntry>,
}

impl Registry {
    pub fn load() -> Result<Self, OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        Self::load_from_path(&config::projects_registry_path()?, &xdg_root)
    }

    #[allow(dead_code)]
    pub fn save(&self) -> Result<(), OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        self.save_to_path(&config::projects_registry_path()?, &xdg_root)
    }

    fn load_from_path(path: &Path, approved_root: &Path) -> Result<Self, OneupError> {
        let path = validate_regular_file_path(path, approved_root).map_err(|err| {
            DaemonError::WatcherError(format!("failed to validate registry path: {err}"))
        })?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| DaemonError::WatcherError(format!("failed to read registry: {e}")))?;

        let registry: Registry = serde_json::from_str(&content)
            .map_err(|e| DaemonError::WatcherError(format!("failed to parse registry: {e}")))?;

        Ok(registry)
    }

    fn save_to_path(&self, path: &Path, approved_root: &Path) -> Result<(), OneupError> {
        let content = serde_json::to_vec_pretty(self)
            .map_err(|e| DaemonError::WatcherError(format!("failed to serialize registry: {e}")))?;
        atomic_replace(
            path,
            &content,
            approved_root,
            XDG_STATE_DIR_MODE,
            SECURE_STATE_FILE_MODE,
        )
        .map_err(|e| DaemonError::WatcherError(format!("failed to write registry: {e}")))?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn register(
        &mut self,
        project_id: &str,
        project_root: &Path,
        indexing: Option<IndexingConfig>,
    ) -> Result<(), OneupError> {
        self.register_with_source(project_id, project_root, project_root, indexing)
    }

    pub fn register_with_source(
        &mut self,
        project_id: &str,
        project_root: &Path,
        source_root: &Path,
        indexing: Option<IndexingConfig>,
    ) -> Result<(), OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        self.register_at_path(
            project_id,
            project_root,
            source_root,
            indexing,
            &config::projects_registry_path()?,
            &xdg_root,
        )
    }

    pub fn deregister(&mut self, project_root: &Path) -> Result<bool, OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        self.deregister_at_path(project_root, &config::projects_registry_path()?, &xdg_root)
    }

    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    pub fn indexing_config_for(&self, project_root: &Path) -> Option<&IndexingConfig> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());

        self.projects
            .iter()
            .find(|project| project.project_root == canonical)
            .and_then(|project| project.indexing.as_ref())
    }

    #[allow(dead_code)]
    pub fn project_roots(&self) -> Vec<PathBuf> {
        self.projects
            .iter()
            .map(|p| p.project_root.clone())
            .collect()
    }

    fn register_at_path(
        &mut self,
        project_id: &str,
        project_root: &Path,
        source_root: &Path,
        indexing: Option<IndexingConfig>,
        path: &Path,
        approved_root: &Path,
    ) -> Result<(), OneupError> {
        let _lock = acquire_registry_lock(approved_root)?;
        let mut latest = Self::load_from_path(path, approved_root)?;
        latest.upsert_project(project_id, project_root, source_root, indexing);
        latest.save_to_path(path, approved_root)?;
        *self = latest;
        Ok(())
    }

    fn deregister_at_path(
        &mut self,
        project_root: &Path,
        path: &Path,
        approved_root: &Path,
    ) -> Result<bool, OneupError> {
        let _lock = acquire_registry_lock(approved_root)?;
        let mut latest = Self::load_from_path(path, approved_root)?;
        let removed = latest.remove_project(project_root);
        if removed {
            latest.save_to_path(path, approved_root)?;
        }
        *self = latest;
        Ok(removed)
    }

    fn upsert_project(
        &mut self,
        project_id: &str,
        project_root: &Path,
        source_root: &Path,
        indexing: Option<IndexingConfig>,
    ) {
        let canonical = canonical_project_root(project_root);
        let canonical_source = canonical_project_root(source_root);
        let source_root = (canonical_source != canonical).then_some(canonical_source);

        if let Some(existing) = self
            .projects
            .iter_mut()
            .find(|project| project.project_root == canonical)
        {
            existing.project_id = project_id.to_string();
            existing.source_root = source_root;
            if let Some(indexing) = indexing {
                existing.indexing = Some(indexing);
            }
            return;
        }

        self.projects.push(ProjectEntry {
            project_id: project_id.to_string(),
            project_root: canonical,
            source_root,
            registered_at: chrono::Utc::now().to_rfc3339(),
            indexing,
        });
    }

    fn remove_project(&mut self, project_root: &Path) -> bool {
        let canonical = canonical_project_root(project_root);
        let before = self.projects.len();
        self.projects.retain(|p| p.project_root != canonical);
        self.projects.len() < before
    }
}

#[cfg(unix)]
struct RegistryLock {
    _lock: Flock<File>,
}

#[cfg(not(unix))]
struct RegistryLock;

#[cfg(unix)]
fn acquire_registry_lock(approved_root: &Path) -> Result<RegistryLock, OneupError> {
    let lock_path =
        validate_regular_file_path(&approved_root.join(REGISTRY_LOCK_FILE), approved_root)
            .map_err(|err| {
                DaemonError::WatcherError(format!("failed to validate registry lock path: {err}"))
            })?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(SECURE_STATE_FILE_MODE)
        .open(&lock_path)
        .map_err(|err| DaemonError::WatcherError(format!("failed to open registry lock: {err}")))?;
    fs::set_permissions(
        &lock_path,
        fs::Permissions::from_mode(SECURE_STATE_FILE_MODE),
    )
    .map_err(|err| DaemonError::WatcherError(format!("failed to secure registry lock: {err}")))?;

    Flock::lock(file, FlockArg::LockExclusive)
        .map(|lock| RegistryLock { _lock: lock })
        .map_err(|(_, errno)| {
            DaemonError::WatcherError(format!("failed to lock registry: {errno}")).into()
        })
}

#[cfg(not(unix))]
fn acquire_registry_lock(_approved_root: &Path) -> Result<RegistryLock, OneupError> {
    Ok(RegistryLock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[cfg(unix)]
    fn mode_bits(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn registry_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("projects.json");

        let project_dir = tmp.path().join("myproject");
        fs::create_dir_all(&project_dir).unwrap();

        let mut reg = Registry::default();
        reg.projects.push(ProjectEntry {
            project_id: "abc-123".to_string(),
            project_root: project_dir.clone(),
            source_root: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: Some(IndexingConfig::new(4, 2, 1).unwrap()),
        });

        let content = serde_json::to_string_pretty(&reg).unwrap();
        fs::write(&registry_path, &content).unwrap();

        let loaded: Registry =
            serde_json::from_str(&fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "abc-123");
        assert_eq!(
            loaded.projects[0].indexing,
            Some(IndexingConfig::new(4, 2, 1).unwrap())
        );
    }

    #[test]
    fn deregister_removes_project() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let mut reg = Registry::default();
        reg.projects.push(ProjectEntry {
            project_id: "id-a".to_string(),
            project_root: dir_a.canonicalize().unwrap(),
            source_root: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: None,
        });
        reg.projects.push(ProjectEntry {
            project_id: "id-b".to_string(),
            project_root: dir_b.canonicalize().unwrap(),
            source_root: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: None,
        });

        let before = reg.projects.len();
        let canonical_a = dir_a.canonicalize().unwrap();
        reg.projects.retain(|p| p.project_root != canonical_a);
        assert_eq!(reg.projects.len(), before - 1);
        assert_eq!(reg.projects[0].project_id, "id-b");
    }

    #[test]
    fn empty_registry() {
        let reg = Registry::default();
        assert!(reg.is_empty());
        assert!(reg.project_roots().is_empty());
    }

    #[test]
    fn registry_deserializes_older_entries_without_indexing() {
        let raw = r#"
        {
          "projects": [
            {
              "project_id": "abc-123",
              "project_root": "/tmp/project",
              "registered_at": "2026-01-01T00:00:00Z"
            }
          ]
        }
        "#;

        let loaded: Registry = serde_json::from_str(raw).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert!(loaded.projects[0].indexing.is_none());
    }

    #[test]
    fn registry_defaults_missing_indexing_fields() {
        let raw = r#"
        {
          "projects": [
            {
              "project_id": "abc-123",
              "project_root": "/tmp/project",
              "registered_at": "2026-01-01T00:00:00Z",
              "indexing": {
                "jobs": 2
              }
            }
          ]
        }
        "#;

        let loaded: Registry = serde_json::from_str(raw).unwrap();
        let indexing = loaded.projects[0].indexing.as_ref().unwrap();
        assert_eq!(indexing.jobs, 2);
        assert_eq!(indexing.embed_threads, 2);
        assert_eq!(
            indexing.write_batch_files,
            crate::shared::types::IndexingConfig::default_write_batch_files_for(indexing.jobs)
        );
    }

    #[test]
    fn registry_save_secures_xdg_root_and_registry_file() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().canonicalize().unwrap().join("xdg-root");
        let registry_path = xdg_root.join("projects.json");

        fs::create_dir_all(&xdg_root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&xdg_root, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let project_root = tmp.path().join("project");
        fs::create_dir_all(&project_root).unwrap();

        let mut registry = Registry::default();
        registry.projects.push(ProjectEntry {
            project_id: "abc-123".to_string(),
            project_root,
            source_root: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: Some(IndexingConfig::new(4, 2, 1).unwrap()),
        });

        registry.save_to_path(&registry_path, &xdg_root).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(mode_bits(&xdg_root), XDG_STATE_DIR_MODE);
            assert_eq!(mode_bits(&registry_path), SECURE_STATE_FILE_MODE);
        }
        assert_eq!(
            Registry::load_from_path(&registry_path, &xdg_root)
                .unwrap()
                .projects
                .len(),
            1
        );
    }

    #[test]
    fn registry_register_reloads_locked_state_before_saving() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project_a = tmp_root.join("project-a");
        let project_b = tmp_root.join("project-b");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();

        let mut first = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        let mut stale_second = first.clone();

        first
            .register_at_path(
                "id-a",
                &project_a,
                &project_a,
                None,
                &registry_path,
                &xdg_root,
            )
            .unwrap();
        stale_second
            .register_at_path(
                "id-b",
                &project_b,
                &project_b,
                None,
                &registry_path,
                &xdg_root,
            )
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 2);
        assert!(loaded
            .projects
            .iter()
            .any(|entry| entry.project_id == "id-a"));
        assert!(loaded
            .projects
            .iter()
            .any(|entry| entry.project_id == "id-b"));
        assert_eq!(stale_second.projects.len(), 2);
    }

    #[test]
    fn registry_register_keeps_one_entry_for_same_project_under_stale_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("project");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();

        let mut first = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        let mut stale_second = first.clone();

        first
            .register_at_path("id-a", &project, &project, None, &registry_path, &xdg_root)
            .unwrap();
        stale_second
            .register_at_path("id-b", &project, &project, None, &registry_path, &xdg_root)
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "id-b");
        assert_eq!(stale_second.projects.len(), 1);
    }

    #[test]
    fn registry_register_persists_distinct_source_root() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        let source = tmp_root.join("worktree");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&source).unwrap();

        let mut registry = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        registry
            .register_at_path("id", &project, &source, None, &registry_path, &xdg_root)
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_root, project);
        assert_eq!(loaded.projects[0].source_root, Some(source));
    }

    #[cfg(unix)]
    #[test]
    fn registry_load_rejects_symlinked_registry_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let outside_root = tmp_root.join("outside");
        let registry_path = xdg_root.join("projects.json");

        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&outside_root).unwrap();
        fs::write(outside_root.join("projects.json"), "{}").unwrap();
        symlink(outside_root.join("projects.json"), &registry_path).unwrap();

        let err = Registry::load_from_path(&registry_path, &xdg_root).unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }
}
