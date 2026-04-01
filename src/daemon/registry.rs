use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::shared::config;
use crate::shared::errors::{DaemonError, OneupError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub project_id: String,
    pub project_root: PathBuf,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Registry {
    pub projects: Vec<ProjectEntry>,
}

impl Registry {
    pub fn load() -> Result<Self, OneupError> {
        let path = config::projects_registry_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| DaemonError::WatcherError(format!("failed to read registry: {e}")))?;

        let registry: Registry = serde_json::from_str(&content)
            .map_err(|e| DaemonError::WatcherError(format!("failed to parse registry: {e}")))?;

        Ok(registry)
    }

    pub fn save(&self) -> Result<(), OneupError> {
        let path = config::projects_registry_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DaemonError::WatcherError(format!("failed to create registry dir: {e}"))
            })?;
        }

        let content = serde_json::to_string_pretty(self)
            .map_err(|e| DaemonError::WatcherError(format!("failed to serialize registry: {e}")))?;

        std::fs::write(&path, content)
            .map_err(|e| DaemonError::WatcherError(format!("failed to write registry: {e}")))?;

        Ok(())
    }

    pub fn register(&mut self, project_id: &str, project_root: &Path) -> Result<(), OneupError> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());

        if self.projects.iter().any(|p| p.project_root == canonical) {
            return Ok(());
        }

        self.projects.push(ProjectEntry {
            project_id: project_id.to_string(),
            project_root: canonical,
            registered_at: chrono::Utc::now().to_rfc3339(),
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
        });

        let content = serde_json::to_string_pretty(&reg).unwrap();
        fs::write(&registry_path, &content).unwrap();

        let loaded: Registry =
            serde_json::from_str(&fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "abc-123");
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
        });
        reg.projects.push(ProjectEntry {
            project_id: "id-b".to_string(),
            project_root: dir_b.canonicalize().unwrap(),
            registered_at: "2026-01-01T00:00:00Z".to_string(),
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
}
