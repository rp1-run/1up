use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, warn};

use crate::indexer::scan_filter::ScanFilter;
use crate::shared::config;
use crate::shared::constants::WATCHER_DEBOUNCE_MS;
use crate::shared::errors::{DaemonError, OneupError};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatcherChanges {
    pub file_paths: BTreeSet<PathBuf>,
    pub ambiguous_paths: BTreeSet<PathBuf>,
    pub has_unscoped_error: bool,
}

impl WatcherChanges {
    pub fn is_empty(&self) -> bool {
        self.file_paths.is_empty() && self.ambiguous_paths.is_empty() && !self.has_unscoped_error
    }
}

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

    pub fn drain_events(&self) -> WatcherChanges {
        let timeout = Duration::from_millis(WATCHER_DEBOUNCE_MS);
        let mut changed = WatcherChanges::default();

        while let Ok(result) = self.rx.recv_timeout(timeout) {
            collect_event_paths(result, &mut changed);
        }

        changed
    }

    pub fn drain_events_nowait(&self) -> WatcherChanges {
        let mut changed = WatcherChanges::default();

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

fn collect_event_paths(result: notify::Result<Event>, changed: &mut WatcherChanges) {
    match result {
        Ok(event) => {
            if event.paths.is_empty() {
                changed.has_unscoped_error = true;
                return;
            }

            for path in event.paths {
                if path.is_file() || !path.exists() {
                    changed.file_paths.insert(path);
                } else if path.is_dir() {
                    changed.ambiguous_paths.insert(path);
                }
            }
        }
        Err(e) => {
            warn!("watcher event error: {e}");
            changed.has_unscoped_error = true;
        }
    }
}

/// Default watcher-level exclude globs: build/dependency directories not
/// already covered by `ScanFilter`'s dotfile-hiding (`.git`/`.1up`/`.rp1`
/// are dotfiles and excluded by default) plus common binary extensions.
/// Fed into the shared `ScanFilter` as the resolved `IndexingConfig`'s
/// exclude globs so the watcher no longer keeps its own drift-prone list;
/// secret-file exclusion (`*.pem`, `*.key`, `credentials.json`, `.env`) is
/// covered unconditionally by `ScanFilter` itself.
const DEFAULT_WATCHER_EXCLUDE_GLOBS: &[&str] = &[
    "**/node_modules/**",
    "**/target/**",
    "**/vendor/**",
    "**/build/**",
    "**/dist/**",
    "**/__pycache__/**",
    "*.png",
    "*.jpg",
    "*.jpeg",
    "*.gif",
    "*.zip",
    "*.tar",
    "*.gz",
    "*.exe",
    "*.dll",
    "*.so",
    "*.dylib",
    "*.bin",
    "*.wasm",
    "*.pyc",
    "*.db",
    "*.sqlite",
    "*.lock",
];

fn default_scan_filter() -> &'static ScanFilter {
    static FILTER: OnceLock<ScanFilter> = OnceLock::new();
    FILTER.get_or_init(|| {
        let exclude_globs: Vec<String> = DEFAULT_WATCHER_EXCLUDE_GLOBS
            .iter()
            .map(|glob| glob.to_string())
            .collect();
        let resolved = config::resolve_indexing_config_with_globs(
            None,
            None,
            None,
            Some(exclude_globs),
            None,
            None,
        )
        .expect("default watcher indexing config resolves");
        ScanFilter::new(
            &resolved.include_globs,
            &resolved.exclude_globs,
            &resolved.index_hidden_dirs,
        )
        .expect("default watcher scan filter compiles")
    })
}

fn should_skip_path(path: &Path) -> bool {
    default_scan_filter().is_excluded(path, false)
}

pub fn filter_changed_paths(changes: WatcherChanges) -> WatcherChanges {
    let file_paths = changes
        .file_paths
        .into_iter()
        .filter(|path| !should_skip_path(path))
        .collect();
    let ambiguous_paths = changes
        .ambiguous_paths
        .into_iter()
        .filter(|path| !should_skip_path(path))
        .collect();

    WatcherChanges {
        file_paths,
        ambiguous_paths,
        has_unscoped_error: changes.has_unscoped_error,
    }
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
    fn skip_secret_pattern_credentials_json() {
        assert!(should_skip_path(Path::new("/project/credentials.json")));
        assert!(should_skip_path(Path::new("/project/secrets/id_rsa.pem")));
        assert!(should_skip_path(Path::new("/project/service.key")));
        assert!(should_skip_path(Path::new("/project/.env")));
    }

    #[test]
    fn allow_source_files() {
        assert!(!should_skip_path(Path::new("/project/src/main.rs")));
        assert!(!should_skip_path(Path::new("/project/lib.py")));
    }

    #[test]
    fn filter_removes_skipped() {
        let filtered = filter_changed_paths(WatcherChanges {
            file_paths: BTreeSet::from([
                PathBuf::from("/p/src/main.rs"),
                PathBuf::from("/p/.git/HEAD"),
                PathBuf::from("/p/lib.py"),
                PathBuf::from("/p/image.png"),
            ]),
            ambiguous_paths: BTreeSet::new(),
            has_unscoped_error: false,
        });
        assert_eq!(filtered.file_paths.len(), 2);
        assert!(filtered.file_paths.iter().all(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            name == "main.rs" || name == "lib.py"
        }));
    }

    #[test]
    fn filter_preserves_ambiguous_paths_and_errors() {
        let filtered = filter_changed_paths(WatcherChanges {
            file_paths: BTreeSet::new(),
            ambiguous_paths: BTreeSet::from([PathBuf::from("/p/src"), PathBuf::from("/p/.git")]),
            has_unscoped_error: true,
        });

        assert_eq!(
            filtered.ambiguous_paths,
            BTreeSet::from([PathBuf::from("/p/src")])
        );
        assert!(filtered.has_unscoped_error);
    }

    #[test]
    fn watcher_creation() {
        let watcher = FileWatcher::new();
        assert!(watcher.is_ok());
        assert!(watcher.unwrap().watched_roots().is_empty());
    }
}
