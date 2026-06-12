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
    active: bool,
    current_active: bool,
    verified_active: bool,
    _lock: MutexGuard<'static, ()>,
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

        let active = model_path.exists();
        if active {
            fs::rename(&model_path, &hidden_path).unwrap();
        }
        let current_active = current_path.exists();
        if current_active {
            fs::rename(&current_path, &hidden_current_path).unwrap();
        }
        // Hide the verified artifact store too: resolution self-heals from
        // intact verified artifacts when the pointer is missing, so leaving
        // it visible would re-enable the model under this guard.
        let verified_active = verified_path.exists();
        if verified_active {
            fs::rename(&verified_path, &hidden_verified_path).unwrap();
        }
        // Create download failure marker to prevent auto-download during tests
        let _ = fs::write(&marker_path, "hidden_by_test");

        Self {
            model_path,
            hidden_path,
            current_path,
            hidden_current_path,
            verified_path,
            hidden_verified_path,
            marker_path,
            active,
            current_active,
            verified_active,
            _lock: lock,
        }
    }
}

impl Drop for HideModelGuard {
    fn drop(&mut self) {
        if self.active && self.hidden_path.exists() {
            let _ = fs::rename(&self.hidden_path, &self.model_path);
        }
        if self.current_active && self.hidden_current_path.exists() {
            let _ = fs::rename(&self.hidden_current_path, &self.current_path);
        }
        if self.verified_active && self.hidden_verified_path.exists() {
            let _ = fs::rename(&self.hidden_verified_path, &self.verified_path);
        }
        let _ = fs::remove_file(&self.marker_path);
    }
}
