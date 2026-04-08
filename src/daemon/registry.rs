use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::shared::config;
use crate::shared::constants::{SECURE_STATE_FILE_MODE, XDG_STATE_DIR_MODE};
use crate::shared::errors::{DaemonError, OneupError};
use crate::shared::fs::{atomic_replace, ensure_secure_xdg_root, validate_regular_file_path};
use crate::shared::types::IndexingConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub project_id: String,
    pub project_root: PathBuf,
    pub registered_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<IndexingConfig>,
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

    pub fn register(
        &mut self,
        project_id: &str,
        project_root: &Path,
        indexing: Option<IndexingConfig>,
    ) -> Result<(), OneupError> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());

        if let Some(existing) = self
            .projects
            .iter_mut()
            .find(|project| project.project_root == canonical)
        {
            existing.project_id = project_id.to_string();
            if let Some(indexing) = indexing {
                existing.indexing = Some(indexing);
            }
            self.save()?;
            return Ok(());
        }

        self.projects.push(ProjectEntry {
            project_id: project_id.to_string(),
            project_root: canonical,
            registered_at: chrono::Utc::now().to_rfc3339(),
            indexing,
        });

        self.save()
    }

    pub fn deregister(&mut self, project_root: &Path) -> Result<bool, OneupError> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());

        let before = self.projects.len();
        self.projects.retain(|p| p.project_root != canonical);
        let removed = self.projects.len() < before;

        if removed {
            self.save()?;
        }

        Ok(removed)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

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
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: None,
        });
        reg.projects.push(ProjectEntry {
            project_id: "id-b".to_string(),
            project_root: dir_b.canonicalize().unwrap(),
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
        fs::set_permissions(&xdg_root, fs::Permissions::from_mode(0o755)).unwrap();

        let project_root = tmp.path().join("project");
        fs::create_dir_all(&project_root).unwrap();

        let mut registry = Registry::default();
        registry.projects.push(ProjectEntry {
            project_id: "abc-123".to_string(),
            project_root,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: Some(IndexingConfig::new(4, 2, 1).unwrap()),
        });

        registry.save_to_path(&registry_path, &xdg_root).unwrap();
        let root_mode = fs::metadata(&xdg_root).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&registry_path).unwrap().permissions().mode() & 0o777;

        assert_eq!(root_mode, XDG_STATE_DIR_MODE);
        assert_eq!(file_mode, SECURE_STATE_FILE_MODE);
        assert_eq!(
            Registry::load_from_path(&registry_path, &xdg_root)
                .unwrap()
                .projects
                .len(),
            1
        );
    }

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
