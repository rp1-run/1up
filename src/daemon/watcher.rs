use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, warn};

use crate::shared::constants::WATCHER_DEBOUNCE_MS;
use crate::shared::errors::{DaemonError, OneupError};

pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<Event>>,
    watched_roots: HashSet<PathBuf>,
}

impl FileWatcher {
    pub fn new() -> Result<Self, OneupError> {
        let (tx, rx) = mpsc::channel();

        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            Config::default().with_poll_interval(Duration::from_secs(2)),
        )
        .map_err(|e| DaemonError::WatcherError(format!("failed to create watcher: {e}")))?;

        Ok(Self {
            _watcher: watcher,
            rx,
            watched_roots: HashSet::new(),
        })
    }

    pub fn watch(&mut self, path: &Path) -> Result<(), OneupError> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if self.watched_roots.contains(&canonical) {
            return Ok(());
        }

        self._watcher
            .watch(&canonical, RecursiveMode::Recursive)
            .map_err(|e| {
                DaemonError::WatcherError(format!("failed to watch {}: {e}", canonical.display()))
            })?;

        self.watched_roots.insert(canonical.clone());
        debug!("watching: {}", canonical.display());
        Ok(())
    }

    pub fn unwatch(&mut self, path: &Path) -> Result<(), OneupError> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if self.watched_roots.remove(&canonical) {
            self._watcher.unwatch(&canonical).map_err(|e| {
                DaemonError::WatcherError(format!("failed to unwatch {}: {e}", canonical.display()))
            })?;
            debug!("unwatched: {}", canonical.display());
        }

        Ok(())
    }

    pub fn unwatch_all(&mut self) -> Result<(), OneupError> {
        let roots: Vec<PathBuf> = self.watched_roots.drain().collect();
        for root in roots {
            if let Err(e) = self._watcher.unwatch(&root) {
                warn!("failed to unwatch {}: {e}", root.display());
            }
        }
        Ok(())
    }

    pub fn drain_events(&self) -> HashSet<PathBuf> {
        let timeout = Duration::from_millis(WATCHER_DEBOUNCE_MS);
        let mut changed = HashSet::new();

        while let Ok(result) = self.rx.recv_timeout(timeout) {
            collect_event_paths(result, &mut changed);
        }

        changed
    }

    pub fn drain_events_nowait(&self) -> HashSet<PathBuf> {
        let mut changed = HashSet::new();

        while let Ok(result) = self.rx.try_recv() {
            collect_event_paths(result, &mut changed);
        }

        changed
    }

    #[allow(dead_code)]
    pub fn watched_roots(&self) -> &HashSet<PathBuf> {
        &self.watched_roots
    }
}

fn collect_event_paths(result: notify::Result<Event>, changed: &mut HashSet<PathBuf>) {
    match result {
        Ok(event) => {
            for path in event.paths {
                if path.is_file() || !path.exists() {
                    changed.insert(path);
                }
            }
        }
        Err(e) => {
            warn!("watcher event error: {e}");
        }
    }
}

fn should_skip_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    let skip_dirs = [
        "node_modules",
        ".git",
        "target",
        "vendor",
        "build",
        "dist",
        "__pycache__",
        ".1up",
        ".rp1",
    ];

    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if skip_dirs.iter().any(|d| s == *d) {
            return true;
        }
    }

    let binary_exts = [
        "png", "jpg", "jpeg", "gif", "zip", "tar", "gz", "exe", "dll", "so", "dylib", "bin",
        "wasm", "pyc", "db", "sqlite", "lock",
    ];

    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if binary_exts.contains(&ext.to_lowercase().as_str()) {
            return true;
        }
    }

    drop(path_str);
    false
}

pub fn filter_changed_paths(paths: HashSet<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| !should_skip_path(p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_gitdir() {
        assert!(should_skip_path(Path::new("/project/.git/objects/abc")));
    }

    #[test]
    fn skip_node_modules() {
        assert!(should_skip_path(Path::new(
            "/project/node_modules/pkg/index.js"
        )));
    }

    #[test]
    fn skip_binary_ext() {
        assert!(should_skip_path(Path::new("/project/image.png")));
    }

    #[test]
    fn allow_source_files() {
        assert!(!should_skip_path(Path::new("/project/src/main.rs")));
        assert!(!should_skip_path(Path::new("/project/lib.py")));
    }

    #[test]
    fn filter_removes_skipped() {
        let paths: HashSet<PathBuf> = [
            PathBuf::from("/p/src/main.rs"),
            PathBuf::from("/p/.git/HEAD"),
            PathBuf::from("/p/lib.py"),
            PathBuf::from("/p/image.png"),
        ]
        .into_iter()
        .collect();

        let filtered = filter_changed_paths(paths);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            name == "main.rs" || name == "lib.py"
        }));
    }

    #[test]
    fn watcher_creation() {
        let watcher = FileWatcher::new();
        assert!(watcher.is_ok());
        assert!(watcher.unwrap().watched_roots().is_empty());
    }
}
