use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use ort::session::Session;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;
use tokio::io::AsyncWriteExt;

use crate::shared::config::{
    download_failure_marker, model_current_manifest_path, model_dir, model_staging_dir,
    model_verified_dir, verified_model_artifact_dir, verified_model_manifest_path,
};
use crate::shared::constants::{
    EMBEDDING_BATCH_SIZE, EMBEDDING_DIM, EMBEDDING_MAX_TOKENS, HF_BASE_URL, HF_MODEL_REPO,
    MODEL_ARTIFACT_MANIFEST_FILENAME, MODEL_ARTIFACT_MANIFEST_VERSION,
    MODEL_CURRENT_MANIFEST_FILENAME, MODEL_DOWNLOAD_CONNECT_TIMEOUT_SECS,
    MODEL_DOWNLOAD_TIMEOUT_SECS, MODEL_FILENAME, MODEL_ONNX_SHA256, MODEL_STAGING_DIRNAME,
    MODEL_VERIFIED_DIRNAME, SECURE_STATE_FILE_MODE, TOKENIZER_FILENAME, TOKENIZER_SHA256,
    XDG_STATE_DIR_MODE,
};
use crate::shared::errors::{EmbeddingError, OneupError};
use crate::shared::fs::{
    atomic_replace, ensure_secure_dir_within_root, ensure_secure_xdg_root, remove_regular_file,
    validate_regular_file_path,
};
use crate::shared::progress::{ProgressState, ProgressUi};

const MODEL_DOWNLOAD_URL: &str = "onnx/model.onnx";
const TOKENIZER_DOWNLOAD_URL: &str = "tokenizer.json";

struct ExpectedArtifactFile {
    filename: &'static str,
    relative_url: &'static str,
    sha256: &'static str,
    label: &'static str,
}

impl ExpectedArtifactFile {
    fn source_url(&self) -> String {
        format!(
            "{}/{}/resolve/main/{}",
            HF_BASE_URL, HF_MODEL_REPO, self.relative_url
        )
    }
}

const EXPECTED_ARTIFACT_FILES: [ExpectedArtifactFile; 2] = [
    ExpectedArtifactFile {
        filename: MODEL_FILENAME,
        relative_url: MODEL_DOWNLOAD_URL,
        sha256: MODEL_ONNX_SHA256,
        label: "model",
    },
    ExpectedArtifactFile {
        filename: TOKENIZER_FILENAME,
        relative_url: TOKENIZER_DOWNLOAD_URL,
        sha256: TOKENIZER_SHA256,
        label: "tokenizer",
    },
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct VerifiedArtifactFile {
    filename: String,
    sha256: String,
    source_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct VerifiedArtifactManifest {
    version: u32,
    artifact_id: String,
    files: Vec<VerifiedArtifactFile>,
}

impl VerifiedArtifactManifest {
    fn for_artifact(artifact_id: String) -> Self {
        Self {
            version: MODEL_ARTIFACT_MANIFEST_VERSION,
            artifact_id,
            files: EXPECTED_ARTIFACT_FILES
                .iter()
                .map(|artifact| VerifiedArtifactFile {
                    filename: artifact.filename.to_string(),
                    sha256: artifact.sha256.to_string(),
                    source_url: artifact.source_url(),
                })
                .collect(),
        }
    }

    fn matches_expected(&self) -> bool {
        if self.version != MODEL_ARTIFACT_MANIFEST_VERSION
            || self.files.len() != EXPECTED_ARTIFACT_FILES.len()
        {
            return false;
        }

        EXPECTED_ARTIFACT_FILES.iter().all(|expected| {
            self.files.iter().any(|file| {
                file.filename == expected.filename
                    && file.sha256 == expected.sha256
                    && file.source_url == expected.source_url()
            })
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ActiveArtifactPointer {
    version: u32,
    artifact_id: String,
}

impl ActiveArtifactPointer {
    fn new(artifact_id: String) -> Self {
        Self {
            version: MODEL_ARTIFACT_MANIFEST_VERSION,
            artifact_id,
        }
    }

    fn is_valid(&self) -> bool {
        self.version == MODEL_ARTIFACT_MANIFEST_VERSION && !self.artifact_id.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    size: u64,
    modified_ns: u128,
}

impl FileFingerprint {
    fn from_path(path: &Path) -> Result<Self, OneupError> {
        let metadata = std::fs::metadata(path).map_err(|e| {
            EmbeddingError::ModelNotAvailable(format!("failed to inspect {}: {e}", path.display()))
        })?;
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        Ok(Self {
            size: metadata.len(),
            modified_ns,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbeddingCompatibilityKey {
    model_dir: PathBuf,
    model: FileFingerprint,
    tokenizer: FileFingerprint,
    embed_threads: usize,
}

impl EmbeddingCompatibilityKey {
    fn from_dir_with_threads(dir: &Path, embed_threads: usize) -> Result<Self, OneupError> {
        let model_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let model_path = model_dir.join(MODEL_FILENAME);
        let tokenizer_path = model_dir.join(TOKENIZER_FILENAME);

        Ok(Self {
            model_dir,
            model: FileFingerprint::from_path(&model_path)?,
            tokenizer: FileFingerprint::from_path(&tokenizer_path)?,
            embed_threads,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingUnavailableReason {
    ModelMissing,
    PreviousDownloadFailed,
    ModelDirUnavailable(String),
    LoadFailed(String),
    DownloadFailed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingLoadStatus {
    Warm,
    Loaded,
    Downloaded,
    Unavailable(EmbeddingUnavailableReason),
}

impl EmbeddingLoadStatus {
    pub fn is_available(&self) -> bool {
        !matches!(self, Self::Unavailable(_))
    }
}

struct CachedRuntime<T> {
    key: EmbeddingCompatibilityKey,
    value: T,
}

struct WarmRuntime<T> {
    cached: Option<CachedRuntime<T>>,
}

impl<T> WarmRuntime<T> {
    fn is_compatible(&self, key: &EmbeddingCompatibilityKey) -> bool {
        self.cached
            .as_ref()
            .is_some_and(|cached| cached.key == *key)
    }

    fn store(&mut self, key: EmbeddingCompatibilityKey, value: T) {
        self.cached = Some(CachedRuntime { key, value });
    }

    fn clear(&mut self) {
        self.cached = None;
    }

    fn current_mut(&mut self) -> Option<&mut T> {
        self.cached.as_mut().map(|cached| &mut cached.value)
    }
}

#[derive(Default)]
pub struct EmbeddingRuntime {
    cache: WarmRuntime<Embedder>,
}

impl<T> Default for WarmRuntime<T> {
    fn default() -> Self {
        Self { cached: None }
    }
}

impl EmbeddingRuntime {
    pub async fn prepare_for_indexing(&mut self, embed_threads: usize) -> EmbeddingLoadStatus {
        self.prepare_for_indexing_with_progress(embed_threads, true)
            .await
    }

    pub async fn prepare_for_indexing_with_progress(
        &mut self,
        embed_threads: usize,
        show_progress_ui: bool,
    ) -> EmbeddingLoadStatus {
        let model_root = match ensure_secure_model_root() {
            Ok(dir) => dir,
            Err(err) => {
                self.cache.clear();
                return EmbeddingLoadStatus::Unavailable(
                    EmbeddingUnavailableReason::ModelDirUnavailable(err.to_string()),
                );
            }
        };

        match resolve_model_dir_without_download(&model_root) {
            Ok(Some(dir)) => return self.prepare_from_model_dir(&dir, embed_threads),
            Ok(None) => {}
            Err(err) => {
                self.cache.clear();
                return EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(
                    err.to_string(),
                ));
            }
        }

        if is_download_failed() {
            self.cache.clear();
            return EmbeddingLoadStatus::Unavailable(
                EmbeddingUnavailableReason::PreviousDownloadFailed,
            );
        }

        self.prepare_with_download(&model_root, embed_threads, show_progress_ui)
            .await
    }

    pub fn prepare_for_search(&mut self, embed_threads: usize) -> EmbeddingLoadStatus {
        let model_root = match ensure_secure_model_root() {
            Ok(dir) => dir,
            Err(err) => {
                self.cache.clear();
                return EmbeddingLoadStatus::Unavailable(
                    EmbeddingUnavailableReason::ModelDirUnavailable(err.to_string()),
                );
            }
        };

        let model_dir = match resolve_model_dir_without_download(&model_root) {
            Ok(Some(dir)) => dir,
            Ok(None) => {
                self.cache.clear();
                return EmbeddingLoadStatus::Unavailable(if is_download_failed() {
                    EmbeddingUnavailableReason::PreviousDownloadFailed
                } else {
                    EmbeddingUnavailableReason::ModelMissing
                });
            }
            Err(err) => {
                self.cache.clear();
                return EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(
                    err.to_string(),
                ));
            }
        };

        self.prepare_from_model_dir(&model_dir, embed_threads)
    }

    pub fn current_embedder(&mut self) -> Option<&mut Embedder> {
        self.cache.current_mut()
    }

    fn prepare_from_model_dir(
        &mut self,
        model_dir: &Path,
        embed_threads: usize,
    ) -> EmbeddingLoadStatus {
        let key = match EmbeddingCompatibilityKey::from_dir_with_threads(model_dir, embed_threads) {
            Ok(key) => key,
            Err(err) => {
                self.cache.clear();
                return EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(
                    err.to_string(),
                ));
            }
        };

        if self.cache.is_compatible(&key) {
            return EmbeddingLoadStatus::Warm;
        }

        match Embedder::from_dir_with_threads(&key.model_dir, embed_threads) {
            Ok(embedder) => {
                self.cache.store(key, embedder);
                EmbeddingLoadStatus::Loaded
            }
            Err(err) => {
                self.cache.clear();
                EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(
                    err.to_string(),
                ))
            }
        }
    }

    async fn prepare_with_download(
        &mut self,
        model_root: &Path,
        embed_threads: usize,
        show_progress_ui: bool,
    ) -> EmbeddingLoadStatus {
        match download_and_activate_verified_artifacts(model_root, show_progress_ui).await {
            Ok(model_dir) => {
                clear_download_failure();
                match self.prepare_from_model_dir(&model_dir, embed_threads) {
                    EmbeddingLoadStatus::Loaded | EmbeddingLoadStatus::Warm => {
                        EmbeddingLoadStatus::Downloaded
                    }
                    status => status,
                }
            }
            Err(err) => {
                mark_download_failed();
                self.cache.clear();
                EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(
                    err.to_string(),
                ))
            }
        }
    }
}

/// Embedding engine backed by an ONNX model (all-MiniLM-L6-v2) with WordPiece tokenization.
///
/// Holds a singleton ONNX session and tokenizer, providing batch inference with
/// mean pooling and L2 normalization to produce 384-dimensional unit vectors.
pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    batch_size: usize,
}

/// Reports whether the embedding model files are present on disk.
///
/// Returns `false` if neither an active verified artifact nor a hash-validated
/// legacy flat-file cache is available.
#[allow(dead_code)]
pub fn is_model_available() -> bool {
    let model_root = match model_dir() {
        Ok(d) => d,
        Err(_) => return false,
    };

    has_active_verified_artifact(&model_root) || legacy_artifacts_match_expected(&model_root)
}

/// Reports whether a previous download attempt failed.
///
/// When true, the system should not re-attempt download automatically.
/// Users can clear the marker by deleting it or running `1up index --retry-download`.
pub fn is_download_failed() -> bool {
    match download_failure_marker() {
        Ok(path) => path.exists(),
        Err(_) => false,
    }
}

/// Writes a download failure marker to prevent automatic retry.
fn mark_download_failed() {
    if let Ok(model_root) = ensure_secure_model_root() {
        if let Ok(marker) = download_failure_marker() {
            let _ = atomic_replace(
                &marker,
                b"download failed",
                &model_root,
                XDG_STATE_DIR_MODE,
                SECURE_STATE_FILE_MODE,
            );
        }
    }
}

/// Clears the download failure marker, allowing a fresh download attempt.
pub fn clear_download_failure() {
    if let Ok(model_root) = model_dir() {
        if let Ok(marker) = download_failure_marker() {
            let _ = remove_regular_file(&marker, &model_root);
        }
    }
}

impl Embedder {
    /// Creates a new embedder, auto-downloading the model if it is not already present.
    ///
    /// The ONNX session is initialized once; reuse this instance across calls.
    #[allow(dead_code)]
    pub async fn new() -> Result<Self, OneupError> {
        Self::with_options(EMBEDDING_BATCH_SIZE, 1).await
    }

    /// Creates a new embedder with a custom ONNX intra-op thread count.
    #[allow(dead_code)]
    pub async fn new_with_threads(intra_threads: usize) -> Result<Self, OneupError> {
        Self::with_options(EMBEDDING_BATCH_SIZE, intra_threads).await
    }

    /// Creates a new embedder with a custom batch size.
    ///
    /// If the model is not present, attempts auto-download. On download failure,
    /// a marker file is written to prevent repeated download attempts.
    #[allow(dead_code)]
    pub async fn with_batch_size(batch_size: usize) -> Result<Self, OneupError> {
        Self::with_options(batch_size, 1).await
    }

    async fn with_options(batch_size: usize, intra_threads: usize) -> Result<Self, OneupError> {
        let model_root = ensure_secure_model_root()?;

        let model_dir = match resolve_model_dir_without_download(&model_root)? {
            Some(dir) => dir,
            None => {
                if is_download_failed() {
                    return Err(EmbeddingError::DownloadFailed(
                        "previous download failed; delete the marker file at ~/.local/share/1up/models/all-MiniLM-L6-v2/.download_failed to retry"
                            .to_string(),
                    )
                    .into());
                }

                match download_and_activate_verified_artifacts(&model_root, true).await {
                    Ok(dir) => {
                        clear_download_failure();
                        dir
                    }
                    Err(err) => {
                        mark_download_failed();
                        return Err(err);
                    }
                }
            }
        };

        Self::from_dir_with_batch_size(&model_dir, intra_threads, batch_size)
    }

    /// Creates an embedder from pre-existing model files at a custom path and thread count.
    pub fn from_dir_with_threads(dir: &Path, intra_threads: usize) -> Result<Self, OneupError> {
        Self::from_dir_with_batch_size(dir, intra_threads, EMBEDDING_BATCH_SIZE)
    }

    fn from_dir_with_batch_size(
        dir: &Path,
        intra_threads: usize,
        batch_size: usize,
    ) -> Result<Self, OneupError> {
        let model_path = dir.join(MODEL_FILENAME);
        let tokenizer_path = dir.join(TOKENIZER_FILENAME);

        if !model_path.exists() {
            return Err(EmbeddingError::ModelNotAvailable(format!(
                "model not found at {}",
                model_path.display()
            ))
            .into());
        }
        if !tokenizer_path.exists() {
            return Err(EmbeddingError::ModelNotAvailable(format!(
                "tokenizer not found at {}",
                tokenizer_path.display()
            ))
            .into());
        }

        let session = Session::builder()
            .map_err(|e| EmbeddingError::InferenceFailed(format!("session builder: {e}")))?
            .with_intra_threads(intra_threads)
            .map_err(|e| EmbeddingError::InferenceFailed(format!("set threads: {e}")))?
            .commit_from_file(&model_path)
            .map_err(|e| EmbeddingError::ModelNotAvailable(format!("failed to load model: {e}")))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            EmbeddingError::TokenizationFailed(format!("failed to load tokenizer: {e}"))
        })?;

        Ok(Self {
            session,
            tokenizer,
            batch_size,
        })
    }

    /// Embeds a single text, returning a 384-dimensional unit vector.
    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>, OneupError> {
        let results = self.embed_batch(&[text])?;
        Ok(results.into_iter().next().unwrap())
    }

    /// Embeds a batch of texts, returning one 384-dimensional unit vector per input.
    ///
    /// Inputs are processed in sub-batches of the configured batch size.
    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, OneupError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());

        for chunk in texts.chunks(self.batch_size) {
            let batch_embeddings = self.run_inference(chunk)?;
            all_embeddings.extend(batch_embeddings);
        }

        Ok(all_embeddings)
    }

    fn run_inference(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, OneupError> {
        let batch_size = texts.len();

        let encodings = texts
            .iter()
            .map(|t| {
                let mut enc = self
                    .tokenizer
                    .encode(*t, true)
                    .map_err(|e| EmbeddingError::TokenizationFailed(e.to_string()))?;
                enc.truncate(
                    EMBEDDING_MAX_TOKENS,
                    0,
                    tokenizers::TruncationDirection::Right,
                );
                Ok(enc)
            })
            .collect::<Result<Vec<_>, OneupError>>()?;

        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);

        let mut input_ids = vec![0i64; batch_size * max_len];
        let mut attention_mask = vec![0i64; batch_size * max_len];
        let mut token_type_ids = vec![0i64; batch_size * max_len];

        for (i, enc) in encodings.iter().enumerate() {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let type_ids = enc.get_type_ids();
            let offset = i * max_len;
            for (j, &id) in ids.iter().enumerate() {
                input_ids[offset + j] = id as i64;
            }
            for (j, &m) in mask.iter().enumerate() {
                attention_mask[offset + j] = m as i64;
            }
            for (j, &t) in type_ids.iter().enumerate() {
                token_type_ids[offset + j] = t as i64;
            }
        }

        let shape = vec![batch_size as i64, max_len as i64];

        let input_ids_tensor = ort::value::Value::from_array((shape.clone(), input_ids.clone()))
            .map_err(|e| EmbeddingError::InferenceFailed(format!("input_ids tensor: {e}")))?;

        let attention_mask_tensor =
            ort::value::Value::from_array((shape.clone(), attention_mask.clone())).map_err(
                |e| EmbeddingError::InferenceFailed(format!("attention_mask tensor: {e}")),
            )?;

        let token_type_ids_tensor = ort::value::Value::from_array((shape, token_type_ids))
            .map_err(|e| EmbeddingError::InferenceFailed(format!("token_type_ids tensor: {e}")))?;

        let inputs = ort::inputs![
            "input_ids" => input_ids_tensor,
            "attention_mask" => attention_mask_tensor,
            "token_type_ids" => token_type_ids_tensor,
        ];

        let outputs = self
            .session
            .run(inputs)
            .map_err(|e| EmbeddingError::InferenceFailed(format!("session run: {e}")))?;

        let output_value = &outputs[0];

        let (out_shape, raw) = output_value
            .try_extract_tensor::<f32>()
            .map_err(|e| EmbeddingError::InferenceFailed(format!("extract tensor: {e}")))?;

        let hidden_dim = *out_shape.last().unwrap_or(&0) as usize;
        let seq_len = if out_shape.len() >= 2 {
            out_shape[1] as usize
        } else {
            0
        };

        if hidden_dim != EMBEDDING_DIM {
            return Err(EmbeddingError::InferenceFailed(format!(
                "expected {EMBEDDING_DIM} dims, got {hidden_dim}"
            ))
            .into());
        }

        let mut embeddings = Vec::with_capacity(batch_size);

        for i in 0..batch_size {
            let mut pooled = vec![0.0f32; EMBEDDING_DIM];
            let mut mask_sum = 0.0f32;

            for j in 0..seq_len {
                let mask_val = attention_mask[i * max_len + j] as f32;
                if mask_val > 0.0 {
                    mask_sum += mask_val;
                    let base = i * seq_len * hidden_dim + j * hidden_dim;
                    for k in 0..EMBEDDING_DIM {
                        pooled[k] += raw[base + k] * mask_val;
                    }
                }
            }

            if mask_sum > 0.0 {
                for v in pooled.iter_mut() {
                    *v /= mask_sum;
                }
            }

            let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 0.0 {
                for v in pooled.iter_mut() {
                    *v /= norm;
                }
            }

            embeddings.push(pooled);
        }

        Ok(embeddings)
    }
}

fn ensure_secure_model_root() -> Result<PathBuf, OneupError> {
    let xdg_root = ensure_secure_xdg_root()?;
    let model_root = model_dir()?;
    ensure_secure_dir_within_root(&model_root, &xdg_root, XDG_STATE_DIR_MODE)
}

fn verified_dir_path(model_root: &Path) -> PathBuf {
    match model_dir() {
        Ok(configured_root) if configured_root == model_root => {
            model_verified_dir().unwrap_or_else(|_| model_root.join(MODEL_VERIFIED_DIRNAME))
        }
        _ => model_root.join(MODEL_VERIFIED_DIRNAME),
    }
}

fn staging_dir_path(model_root: &Path) -> PathBuf {
    match model_dir() {
        Ok(configured_root) if configured_root == model_root => {
            model_staging_dir().unwrap_or_else(|_| model_root.join(MODEL_STAGING_DIRNAME))
        }
        _ => model_root.join(MODEL_STAGING_DIRNAME),
    }
}

fn current_manifest_path(model_root: &Path) -> PathBuf {
    match model_dir() {
        Ok(configured_root) if configured_root == model_root => model_current_manifest_path()
            .unwrap_or_else(|_| model_root.join(MODEL_CURRENT_MANIFEST_FILENAME)),
        _ => model_root.join(MODEL_CURRENT_MANIFEST_FILENAME),
    }
}

fn artifact_dir_path(model_root: &Path, artifact_id: &str) -> PathBuf {
    match model_dir() {
        Ok(configured_root) if configured_root == model_root => {
            verified_model_artifact_dir(artifact_id)
                .unwrap_or_else(|_| verified_dir_path(model_root).join(artifact_id))
        }
        _ => verified_dir_path(model_root).join(artifact_id),
    }
}

fn manifest_path(model_root: &Path, artifact_id: &str) -> PathBuf {
    match model_dir() {
        Ok(configured_root) if configured_root == model_root => {
            verified_model_manifest_path(artifact_id).unwrap_or_else(|_| {
                artifact_dir_path(model_root, artifact_id).join(MODEL_ARTIFACT_MANIFEST_FILENAME)
            })
        }
        _ => artifact_dir_path(model_root, artifact_id).join(MODEL_ARTIFACT_MANIFEST_FILENAME),
    }
}

fn has_active_verified_artifact(model_root: &Path) -> bool {
    try_load_active_artifact_dir(model_root)
        .map(|dir| dir.is_some())
        .unwrap_or(false)
}

fn legacy_artifacts_match_expected(model_root: &Path) -> bool {
    EXPECTED_ARTIFACT_FILES.iter().all(|artifact| {
        let path = model_root.join(artifact.filename);
        path.exists() && sha256_digest_file(&path).is_ok_and(|digest| digest == artifact.sha256)
    })
}

fn resolve_model_dir_without_download(model_root: &Path) -> Result<Option<PathBuf>, OneupError> {
    if let Some(active_dir) = try_load_active_artifact_dir(model_root)? {
        return Ok(Some(active_dir));
    }

    try_activate_legacy_artifacts(model_root)
}

fn try_load_active_artifact_dir(model_root: &Path) -> Result<Option<PathBuf>, OneupError> {
    let current_path = current_manifest_path(model_root);
    let current_bytes = match read_validated_file(&current_path, model_root) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let current: ActiveArtifactPointer =
        match serde_json::from_slice::<ActiveArtifactPointer>(&current_bytes) {
            Ok(pointer) if pointer.is_valid() => pointer,
            _ => return Ok(None),
        };

    let manifest_bytes =
        match read_validated_file(&manifest_path(model_root, &current.artifact_id), model_root) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(None),
        };
    let manifest: VerifiedArtifactManifest =
        match serde_json::from_slice::<VerifiedArtifactManifest>(&manifest_bytes) {
            Ok(manifest)
                if manifest.artifact_id == current.artifact_id && manifest.matches_expected() =>
            {
                manifest
            }
            _ => return Ok(None),
        };

    let artifact_dir = artifact_dir_path(model_root, &manifest.artifact_id);
    for artifact in EXPECTED_ARTIFACT_FILES {
        let path = artifact_dir.join(artifact.filename);
        if validate_regular_file_path(&path, model_root)
            .and_then(|validated| {
                fs::metadata(&validated).map(|_| ()).map_err(|err| {
                    EmbeddingError::ModelNotAvailable(format!(
                        "failed to inspect {}: {err}",
                        validated.display()
                    ))
                    .into()
                })
            })
            .is_err()
        {
            return Ok(None);
        }
    }

    Ok(Some(artifact_dir))
}

fn try_activate_legacy_artifacts(model_root: &Path) -> Result<Option<PathBuf>, OneupError> {
    let legacy_paths: Vec<PathBuf> = EXPECTED_ARTIFACT_FILES
        .iter()
        .map(|artifact| model_root.join(artifact.filename))
        .collect();
    if legacy_paths.iter().any(|path| !path.exists()) {
        return Ok(None);
    }

    for (artifact, path) in EXPECTED_ARTIFACT_FILES.iter().zip(legacy_paths.iter()) {
        let digest = sha256_digest_file(path)?;
        if digest != artifact.sha256 {
            return Ok(None);
        }
    }

    let artifact_id = format!(
        "v{}-{}",
        MODEL_ARTIFACT_MANIFEST_VERSION,
        uuid::Uuid::new_v4().simple()
    );
    let stage_dir = create_stage_dir(model_root, &artifact_id)?;
    let cleanup_path = stage_dir.clone();

    let copy_result = (|| -> Result<PathBuf, OneupError> {
        for (artifact, path) in EXPECTED_ARTIFACT_FILES.iter().zip(legacy_paths.iter()) {
            copy_file_to_stage(path, &stage_dir.join(artifact.filename), artifact.label)?;
        }
        activate_staged_artifact(model_root, &artifact_id, &stage_dir)
    })();

    if copy_result.is_err() {
        let _ = fs::remove_dir_all(cleanup_path);
    }

    copy_result.map(Some)
}

async fn download_and_activate_verified_artifacts(
    model_root: &Path,
    show_progress_ui: bool,
) -> Result<PathBuf, OneupError> {
    let artifact_id = format!(
        "v{}-{}",
        MODEL_ARTIFACT_MANIFEST_VERSION,
        uuid::Uuid::new_v4().simple()
    );
    let stage_dir = create_stage_dir(model_root, &artifact_id)?;
    let cleanup_path = stage_dir.clone();

    let download_result = async {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(MODEL_DOWNLOAD_CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(MODEL_DOWNLOAD_TIMEOUT_SECS))
            .build()
            .map_err(|err| {
                EmbeddingError::DownloadFailed(format!("build download client: {err}"))
            })?;

        for artifact in EXPECTED_ARTIFACT_FILES {
            download_file_to_stage(
                &client,
                &artifact.source_url(),
                &stage_dir.join(artifact.filename),
                artifact.label,
                show_progress_ui,
            )
            .await?;
        }

        activate_staged_artifact(model_root, &artifact_id, &stage_dir)
    }
    .await;

    if download_result.is_err() {
        let _ = fs::remove_dir_all(cleanup_path);
    }

    download_result
}

async fn download_file_to_stage(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
    show_progress_ui: bool,
) -> Result<(), OneupError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| EmbeddingError::DownloadFailed(format!("{label}: {err}")))?;

    if !response.status().is_success() {
        return Err(
            EmbeddingError::DownloadFailed(format!("{label}: HTTP {}", response.status())).into(),
        );
    }

    let total = response.content_length().unwrap_or(0);
    let mut progress_ui =
        ProgressUi::stderr_if(download_progress_state(label, 0, total), show_progress_ui);

    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dest)
        .await
        .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} file create: {err}")))?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;

    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = chunk
            .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} stream: {err}")))?;
        file.write_all(&chunk)
            .await
            .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} write: {err}")))?;
        downloaded += chunk.len() as u64;
        progress_ui.set_state(download_progress_state(label, downloaded, total));
    }

    if total > 0 && downloaded != total {
        return Err(EmbeddingError::DownloadFailed(format!(
            "{label}: incomplete download ({downloaded}/{total} bytes)"
        ))
        .into());
    }

    file.flush()
        .await
        .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} flush: {err}")))?;
    file.sync_all()
        .await
        .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} sync: {err}")))?;
    set_path_mode(dest, SECURE_STATE_FILE_MODE).map_err(|err| {
        EmbeddingError::DownloadFailed(format!("{label} chmod {}: {err}", dest.display()))
    })?;

    progress_ui.success_with(format!("{label} downloaded"));
    Ok(())
}

fn download_progress_state(label: &str, downloaded: u64, total: u64) -> ProgressState {
    let message = format!("Downloading {label}");
    if total > 0 {
        ProgressState::bytes(message, downloaded, total)
    } else {
        ProgressState::spinner(message)
    }
}

fn create_stage_dir(model_root: &Path, artifact_id: &str) -> Result<PathBuf, OneupError> {
    let staging_root = ensure_secure_dir_within_root(
        &staging_dir_path(model_root),
        model_root,
        XDG_STATE_DIR_MODE,
    )?;
    ensure_secure_dir_within_root(
        &staging_root.join(artifact_id),
        model_root,
        XDG_STATE_DIR_MODE,
    )
}

fn copy_file_to_stage(source: &Path, dest: &Path, label: &str) -> Result<(), OneupError> {
    let mut src = File::open(source)
        .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} copy open: {err}")))?;
    let mut dest_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dest)
        .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} copy create: {err}")))?;
    set_path_mode(dest, SECURE_STATE_FILE_MODE).map_err(|err| {
        EmbeddingError::DownloadFailed(format!("{label} copy chmod {}: {err}", dest.display()))
    })?;
    std::io::copy(&mut src, &mut dest_file)
        .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} copy write: {err}")))?;
    dest_file
        .sync_all()
        .map_err(|err| EmbeddingError::DownloadFailed(format!("{label} copy sync: {err}")))?;
    Ok(())
}

fn activate_staged_artifact(
    model_root: &Path,
    artifact_id: &str,
    stage_dir: &Path,
) -> Result<PathBuf, OneupError> {
    for artifact in EXPECTED_ARTIFACT_FILES {
        let staged_path = stage_dir.join(artifact.filename);
        let digest = sha256_digest_file(&staged_path)?;
        if digest != artifact.sha256 {
            return Err(EmbeddingError::DownloadFailed(format!(
                "{} SHA-256 mismatch: expected {}, got {}",
                artifact.label, artifact.sha256, digest
            ))
            .into());
        }
    }

    let manifest = VerifiedArtifactManifest::for_artifact(artifact_id.to_string());
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|err| EmbeddingError::DownloadFailed(format!("serialize manifest: {err}")))?;
    write_stage_file(&stage_dir.join("manifest.json"), &manifest_bytes)?;
    sync_directory(stage_dir)?;

    let verified_root = ensure_secure_dir_within_root(
        &verified_dir_path(model_root),
        model_root,
        XDG_STATE_DIR_MODE,
    )?;
    let final_dir = verified_root.join(artifact_id);
    fs::rename(stage_dir, &final_dir).map_err(|err| {
        EmbeddingError::DownloadFailed(format!(
            "activate verified artifact {}: {err}",
            final_dir.display()
        ))
    })?;
    sync_directory(&verified_root)?;

    let current = ActiveArtifactPointer::new(artifact_id.to_string());
    let current_bytes = serde_json::to_vec_pretty(&current).map_err(|err| {
        EmbeddingError::DownloadFailed(format!("serialize current manifest: {err}"))
    })?;
    atomic_replace(
        &current_manifest_path(model_root),
        &current_bytes,
        model_root,
        XDG_STATE_DIR_MODE,
        SECURE_STATE_FILE_MODE,
    )?;

    Ok(final_dir)
}

fn write_stage_file(path: &Path, contents: &[u8]) -> Result<(), OneupError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|err| {
            EmbeddingError::DownloadFailed(format!("write stage file {}: {err}", path.display()))
        })?;
    set_path_mode(path, SECURE_STATE_FILE_MODE).map_err(|err| {
        EmbeddingError::DownloadFailed(format!("chmod stage file {}: {err}", path.display()))
    })?;
    file.write_all(contents).map_err(|err| {
        EmbeddingError::DownloadFailed(format!("write stage file {}: {err}", path.display()))
    })?;
    file.sync_all().map_err(|err| {
        EmbeddingError::DownloadFailed(format!("sync stage file {}: {err}", path.display()))
    })?;
    Ok(())
}

fn read_validated_file(path: &Path, approved_root: &Path) -> Result<Vec<u8>, OneupError> {
    let validated = validate_regular_file_path(path, approved_root)?;
    fs::read(&validated).map_err(|err| {
        EmbeddingError::ModelNotAvailable(format!("failed to read {}: {err}", validated.display()))
            .into()
    })
}

fn sha256_digest_file(path: &Path) -> Result<String, OneupError> {
    let file = File::open(path).map_err(|err| {
        EmbeddingError::ModelNotAvailable(format!("failed to read {}: {err}", path.display()))
    })?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = reader.read(&mut buf).map_err(|err| {
            EmbeddingError::ModelNotAvailable(format!("failed to hash {}: {err}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn sync_directory(path: &Path) -> Result<(), OneupError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }

    #[cfg(unix)]
    {
        let file = File::open(path).map_err(|err| {
            EmbeddingError::DownloadFailed(format!("open directory {}: {err}", path.display()))
        })?;
        file.sync_all().map_err(|err| {
            EmbeddingError::DownloadFailed(format!("sync directory {}: {err}", path.display()))
                .into()
        })
    }
}

fn set_path_mode(path: &Path, mode: u32) -> Result<(), OneupError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|err| {
            EmbeddingError::DownloadFailed(format!("chmod {}: {err}", path.display())).into()
        })
    }

    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread;

    static MODEL_MUTEX: Mutex<()> = Mutex::new(());

    fn write_fake_model_files(dir: &std::path::Path, model: &[u8], tokenizer: &[u8]) {
        std::fs::write(dir.join(MODEL_FILENAME), model).unwrap();
        std::fs::write(dir.join(TOKENIZER_FILENAME), tokenizer).unwrap();
    }

    fn runtime_model_dir() -> PathBuf {
        let root = model_dir().unwrap();
        resolve_model_dir_without_download(&root)
            .unwrap()
            .expect("model available")
    }

    #[test]
    fn model_availability_check() {
        // Smoke test: verify is_model_available() completes without panicking.
        // The return value depends on whether model files exist on disk.
        let _available = is_model_available();
    }

    #[test]
    fn download_failure_marker_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let marker_path = tmp.path().join(".download_failed");

        assert!(!marker_path.exists());

        std::fs::write(&marker_path, "download failed").unwrap();
        assert!(marker_path.exists());

        std::fs::remove_file(&marker_path).unwrap();
        assert!(!marker_path.exists());
    }

    #[test]
    fn mark_and_clear_download_failure() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        mark_download_failed();
        assert!(is_download_failed());

        clear_download_failure();
        assert!(!is_download_failed());
    }

    #[test]
    fn is_model_available_returns_false_when_files_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!tmp.path().join(MODEL_FILENAME).exists());
        assert!(!tmp.path().join(TOKENIZER_FILENAME).exists());
    }

    #[test]
    fn from_dir_missing_model() {
        let tmp = tempfile::tempdir().unwrap();
        let result = Embedder::from_dir_with_threads(tmp.path(), 1);
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(err.contains("model not found") || err.contains("not found"));
    }

    #[test]
    fn from_dir_missing_tokenizer() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MODEL_FILENAME), b"not a real model").unwrap();
        let result = Embedder::from_dir_with_threads(tmp.path(), 1);
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(err.contains("tokenizer not found") || err.contains("not found"));
    }

    #[test]
    fn compatibility_key_changes_when_embed_threads_change() {
        let tmp = tempfile::tempdir().unwrap();
        write_fake_model_files(tmp.path(), b"model-v1", b"tokenizer-v1");

        let key_a = EmbeddingCompatibilityKey::from_dir_with_threads(tmp.path(), 1).unwrap();
        let key_b = EmbeddingCompatibilityKey::from_dir_with_threads(tmp.path(), 2).unwrap();

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn compatibility_key_changes_when_model_files_change() {
        let tmp = tempfile::tempdir().unwrap();
        write_fake_model_files(tmp.path(), b"model-v1", b"tokenizer-v1");

        let key_before = EmbeddingCompatibilityKey::from_dir_with_threads(tmp.path(), 2).unwrap();

        write_fake_model_files(tmp.path(), b"model-v2-with-different-size", b"tokenizer-v1");

        let key_after = EmbeddingCompatibilityKey::from_dir_with_threads(tmp.path(), 2).unwrap();
        assert_ne!(key_before, key_after);
    }

    #[test]
    fn warm_runtime_reports_only_matching_keys_as_compatible() {
        let tmp = tempfile::tempdir().unwrap();
        write_fake_model_files(tmp.path(), b"model-v1", b"tokenizer-v1");

        let key_a = EmbeddingCompatibilityKey::from_dir_with_threads(tmp.path(), 2).unwrap();
        let key_b = EmbeddingCompatibilityKey::from_dir_with_threads(tmp.path(), 3).unwrap();

        let mut cache = WarmRuntime::default();
        cache.store(key_a.clone(), 7usize);

        assert!(cache.is_compatible(&key_a));
        assert!(!cache.is_compatible(&key_b));
        assert_eq!(cache.current_mut().map(|value| *value), Some(7));
    }

    #[test]
    fn legacy_artifacts_import_into_verified_store_only_after_digest_validation() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().canonicalize().unwrap().join("models");
        std::fs::create_dir_all(&model_root).unwrap();

        let runtime_dir = runtime_model_dir();
        let live_model = std::fs::read(runtime_dir.join(MODEL_FILENAME)).unwrap();
        let live_tokenizer = std::fs::read(runtime_dir.join(TOKENIZER_FILENAME)).unwrap();
        std::fs::write(model_root.join(MODEL_FILENAME), &live_model).unwrap();
        std::fs::write(model_root.join(TOKENIZER_FILENAME), &live_tokenizer).unwrap();

        let activated = try_activate_legacy_artifacts(&model_root)
            .unwrap()
            .expect("legacy artifacts should import");
        let current: ActiveArtifactPointer =
            serde_json::from_slice(&std::fs::read(current_manifest_path(&model_root)).unwrap())
                .unwrap();
        let manifest: VerifiedArtifactManifest = serde_json::from_slice(
            &std::fs::read(manifest_path(&model_root, &current.artifact_id)).unwrap(),
        )
        .unwrap();

        assert_eq!(
            activated,
            artifact_dir_path(&model_root, &current.artifact_id)
        );
        assert!(manifest.matches_expected());
        assert!(activated.join(MODEL_FILENAME).exists());
        assert!(activated.join(TOKENIZER_FILENAME).exists());
    }

    #[test]
    fn invalid_legacy_artifacts_do_not_replace_active_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().join("models");
        std::fs::create_dir_all(&model_root).unwrap();

        let active_id = "active-good";
        let active_dir = artifact_dir_path(&model_root, active_id);
        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::write(active_dir.join(MODEL_FILENAME), b"active-model").unwrap();
        std::fs::write(active_dir.join(TOKENIZER_FILENAME), b"active-tokenizer").unwrap();
        std::fs::write(
            active_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&VerifiedArtifactManifest::for_artifact(
                active_id.to_string(),
            ))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            current_manifest_path(&model_root),
            serde_json::to_vec_pretty(&ActiveArtifactPointer::new(active_id.to_string())).unwrap(),
        )
        .unwrap();

        std::fs::write(model_root.join(MODEL_FILENAME), b"tampered-model").unwrap();
        std::fs::write(model_root.join(TOKENIZER_FILENAME), b"tampered-tokenizer").unwrap();

        let result = try_activate_legacy_artifacts(&model_root).unwrap();
        let current: ActiveArtifactPointer =
            serde_json::from_slice(&std::fs::read(current_manifest_path(&model_root)).unwrap())
                .unwrap();

        assert!(result.is_none());
        assert_eq!(current.artifact_id, active_id);
    }

    #[test]
    fn activate_staged_artifact_keeps_current_manifest_on_digest_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().canonicalize().unwrap().join("models");
        std::fs::create_dir_all(&model_root).unwrap();

        let active_id = "active-good";
        let active_dir = artifact_dir_path(&model_root, active_id);
        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::write(active_dir.join(MODEL_FILENAME), b"active-model").unwrap();
        std::fs::write(active_dir.join(TOKENIZER_FILENAME), b"active-tokenizer").unwrap();
        std::fs::write(
            active_dir.join(MODEL_ARTIFACT_MANIFEST_FILENAME),
            serde_json::to_vec_pretty(&VerifiedArtifactManifest::for_artifact(
                active_id.to_string(),
            ))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            current_manifest_path(&model_root),
            serde_json::to_vec_pretty(&ActiveArtifactPointer::new(active_id.to_string())).unwrap(),
        )
        .unwrap();

        let candidate_id = "candidate-bad";
        let stage_dir = create_stage_dir(&model_root, candidate_id).unwrap();
        std::fs::write(stage_dir.join(MODEL_FILENAME), b"tampered-model").unwrap();
        std::fs::write(stage_dir.join(TOKENIZER_FILENAME), b"tampered-tokenizer").unwrap();

        let err = activate_staged_artifact(&model_root, candidate_id, &stage_dir).unwrap_err();
        let current: ActiveArtifactPointer =
            serde_json::from_slice(&std::fs::read(current_manifest_path(&model_root)).unwrap())
                .unwrap();

        assert!(err.to_string().contains("SHA-256 mismatch"));
        assert_eq!(current.artifact_id, active_id);
        assert!(stage_dir.exists());
        assert!(!artifact_dir_path(&model_root, candidate_id).exists());
    }

    #[tokio::test]
    async fn download_file_to_stage_rejects_partial_http_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc")
                .unwrap();
            stream.flush().unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().join("models");
        std::fs::create_dir_all(&model_root).unwrap();
        let destination = model_root.join(MODEL_FILENAME);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();

        let err = download_file_to_stage(
            &client,
            &format!("http://{address}/model.onnx"),
            &destination,
            "model",
            false,
        )
        .await
        .unwrap_err();

        server.join().unwrap();

        assert!(
            err.to_string().contains("incomplete download") || err.to_string().contains("stream"),
        );
        assert!(!current_manifest_path(&model_root).exists());
    }

    #[test]
    fn prepare_for_search_reuses_warm_runtime_when_model_is_unchanged() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let mut runtime = EmbeddingRuntime::default();
        let first = runtime.prepare_for_search(1);
        assert!(
            matches!(
                first,
                EmbeddingLoadStatus::Loaded | EmbeddingLoadStatus::Downloaded
            ),
            "expected an initial load, got {first:?}"
        );
        assert!(runtime.current_embedder().is_some());

        let second = runtime.prepare_for_search(1);
        assert_eq!(second, EmbeddingLoadStatus::Warm);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn prepare_for_indexing_reuses_warm_runtime_when_model_is_unchanged() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let mut runtime = EmbeddingRuntime::default();
        let first = runtime.prepare_for_indexing(1).await;
        assert!(
            matches!(
                first,
                EmbeddingLoadStatus::Loaded | EmbeddingLoadStatus::Downloaded
            ),
            "expected an initial load, got {first:?}"
        );
        assert!(runtime.current_embedder().is_some());

        let second = runtime.prepare_for_indexing(1).await;
        assert_eq!(second, EmbeddingLoadStatus::Warm);
    }

    #[test]
    fn embed_batch_empty_input() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir_with_threads(&runtime_model_dir(), 1).unwrap();
        let result = embedder.embed_batch(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn embed_one_produces_correct_dim() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir_with_threads(&runtime_model_dir(), 1).unwrap();
        let vec = embedder.embed_one("hello world").unwrap();
        assert_eq!(vec.len(), EMBEDDING_DIM);
    }

    #[test]
    fn embed_one_l2_normalized() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir_with_threads(&runtime_model_dir(), 1).unwrap();
        let vec = embedder.embed_one("the quick brown fox").unwrap();
        let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "L2 norm should be ~1.0, got {norm}"
        );
    }

    #[test]
    fn embed_batch_multiple_texts() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir_with_threads(&runtime_model_dir(), 1).unwrap();
        let texts = vec![
            "error handling in rust",
            "machine learning algorithms",
            "web server configuration",
        ];
        let results = embedder.embed_batch(&texts).unwrap();
        assert_eq!(results.len(), 3);
        for vec in &results {
            assert_eq!(vec.len(), EMBEDDING_DIM);
            let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-4,
                "L2 norm should be ~1.0, got {norm}"
            );
        }
    }

    #[test]
    fn embed_similar_texts_closer_than_dissimilar() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir_with_threads(&runtime_model_dir(), 1).unwrap();
        let vecs = embedder
            .embed_batch(&[
                "how to handle errors in rust",
                "error handling patterns in rust programming",
                "recipe for chocolate cake",
            ])
            .unwrap();

        let cosine =
            |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b.iter()).map(|(x, y)| x * y).sum() };

        let sim_similar = cosine(&vecs[0], &vecs[1]);
        let sim_dissimilar = cosine(&vecs[0], &vecs[2]);
        assert!(
            sim_similar > sim_dissimilar,
            "similar texts should have higher cosine similarity: {sim_similar} vs {sim_dissimilar}"
        );
    }

    #[test]
    fn embed_batch_exceeding_batch_size() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let dir = runtime_model_dir();
        let mut embedder = Embedder {
            session: Session::builder()
                .unwrap()
                .with_intra_threads(1)
                .unwrap()
                .commit_from_file(dir.join(MODEL_FILENAME))
                .unwrap(),
            tokenizer: Tokenizer::from_file(dir.join(TOKENIZER_FILENAME)).unwrap(),
            batch_size: 2,
        };

        let texts: Vec<&str> = vec![
            "text one",
            "text two",
            "text three",
            "text four",
            "text five",
        ];
        let results = embedder.embed_batch(&texts).unwrap();
        assert_eq!(results.len(), 5);
        for vec in &results {
            assert_eq!(vec.len(), EMBEDDING_DIM);
        }
    }
}
