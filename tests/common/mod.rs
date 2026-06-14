//! Shared test-support utilities for the integration test binaries.

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Serializes tests that mutate the shared on-disk embedding model state
/// within one test binary.
pub static MODEL_MUTEX: Mutex<()> = Mutex::new(());

/// RAII guard that temporarily hides the embedding model to force FTS-only
/// mode. On drop, the model is restored. Holds [`MODEL_MUTEX`] to prevent
/// concurrent test interference.
pub struct HideModelGuard {
    model_path: PathBuf,
    hidden_path: PathBuf,
    current_path: PathBuf,
    hidden_current_path: PathBuf,
    verified_path: PathBuf,
    hidden_verified_path: PathBuf,
    marker_path: PathBuf,
    marker_preexisting: Option<Vec<u8>>,
    active: bool,
    current_active: bool,
    verified_active: bool,
    _lock: MutexGuard<'static, ()>,
}

/// Hides `real` at `hidden`, tolerating state leaked by a previous
/// interrupted run. If only the hidden copy exists, the prior run died with
/// the artifact hidden: restore it first so this guard sees the honest
/// pre-test state. If both exist, the hidden copy is a stale duplicate;
/// drop it so a directory rename cannot fail with `DirectoryNotEmpty` and
/// brick every later guard (a mid-construction panic here leaks the
/// already-hidden artifacts because `Drop` never runs).
fn hide_artifact(real: &std::path::Path, hidden: &std::path::Path) -> bool {
    if hidden.exists() {
        if real.exists() {
            let _ = if hidden.is_dir() {
                fs::remove_dir_all(hidden)
            } else {
                fs::remove_file(hidden)
            };
        } else {
            let _ = fs::rename(hidden, real);
        }
    }
    let active = real.exists();
    if active {
        fs::rename(real, hidden).unwrap();
    }
    active
}

/// Restores `real` from `hidden`, replacing any artifact recreated while
/// the guard was active (e.g. by a straggling daemon) so the original wins
/// and no `*.hidden_by_test` state survives the guard.
fn restore_artifact(real: &std::path::Path, hidden: &std::path::Path) {
    if !hidden.exists() {
        return;
    }
    if real.exists() {
        let _ = if real.is_dir() {
            fs::remove_dir_all(real)
        } else {
            fs::remove_file(real)
        };
    }
    let _ = fs::rename(hidden, real);
}

impl HideModelGuard {
    pub fn new() -> Self {
        let lock = MODEL_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let model_dir = dirs::data_dir()
            .unwrap()
            .join("1up")
            .join("models")
            .join("all-MiniLM-L6-v2");
        let _ = fs::create_dir_all(&model_dir);
        let model_path = model_dir.join("model.onnx");
        let hidden_path = model_dir.join("model.onnx.hidden_by_test");
        let current_path = model_dir.join("current.json");
        let hidden_current_path = model_dir.join("current.json.hidden_by_test");
        let verified_path = model_dir.join("verified");
        let hidden_verified_path = model_dir.join("verified.hidden_by_test");
        let marker_path = model_dir.join(".download_failed");

        let active = hide_artifact(&model_path, &hidden_path);
        let current_active = hide_artifact(&current_path, &hidden_current_path);
        // Hide the verified artifact store too: resolution self-heals from
        // intact verified artifacts when the pointer is missing, so leaving
        // it visible would re-enable the model under this guard.
        let verified_active = hide_artifact(&verified_path, &hidden_verified_path);
        // Create download failure marker to prevent auto-download during
        // tests. Record any pre-existing marker first: on model-less
        // machines (CI) an organic marker is what keeps the rest of the
        // suite download-free, so Drop must restore it rather than delete
        // it and re-arm auto-download for later tests.
        let marker_preexisting = fs::read(&marker_path).ok();
        let _ = fs::write(&marker_path, "hidden_by_test");

        Self {
            model_path,
            hidden_path,
            current_path,
            hidden_current_path,
            verified_path,
            hidden_verified_path,
            marker_path,
            marker_preexisting,
            active,
            current_active,
            verified_active,
            _lock: lock,
        }
    }
}

impl Drop for HideModelGuard {
    fn drop(&mut self) {
        if self.active {
            restore_artifact(&self.model_path, &self.hidden_path);
        }
        if self.current_active {
            restore_artifact(&self.current_path, &self.hidden_current_path);
        }
        if self.verified_active {
            restore_artifact(&self.verified_path, &self.hidden_verified_path);
        }
        match &self.marker_preexisting {
            Some(content) => {
                let _ = fs::write(&self.marker_path, content);
            }
            None => {
                let _ = fs::remove_file(&self.marker_path);
            }
        }
    }
}
