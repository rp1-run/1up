use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokenizers::{Encoding, Tokenizer};
use tokio::io::AsyncWriteExt;

use crate::shared::config::{
    download_failure_marker, model_current_manifest_path, model_dir, model_staging_dir,
    model_verified_dir, verified_model_artifact_dir, verified_model_manifest_path,
};
use crate::shared::constants::{
    DISABLE_MODEL_DOWNLOADS_ENV_VAR, EMBEDDING_BATCH_SIZE, EMBEDDING_DIM, EMBEDDING_MAX_TOKENS,
    HF_BASE_URL, HF_MODEL_REPO, MODEL_ARTIFACT_MANIFEST_FILENAME, MODEL_ARTIFACT_MANIFEST_VERSION,
    MODEL_CURRENT_MANIFEST_FILENAME, MODEL_DOWNLOAD_CONNECT_TIMEOUT_SECS,
    MODEL_DOWNLOAD_TIMEOUT_SECS, MODEL_FILENAME, MODEL_ONNX_INT8_FILENAME, MODEL_ONNX_SHA256,
    MODEL_STAGING_DIRNAME, MODEL_VARIANT_ENV_VAR, MODEL_VARIANT_INT8_SUFFIX,
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
    variant: ModelVariant,
    model: FileFingerprint,
    tokenizer: FileFingerprint,
    embed_threads: usize,
}

impl EmbeddingCompatibilityKey {
    fn from_dir_with_variant_and_threads(
        dir: &Path,
        variant: ModelVariant,
        embed_threads: usize,
    ) -> Result<Self, OneupError> {
        let model_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        // Fingerprint the artifact backing the *resolved* variant so a variant
        // swap changes the key (a long-lived daemon can never reuse a warm INT8
        // embedder for an FP32 run or vice versa), on top of the identity change
        // already folded via `ModelVariant::model_id`.
        let model_path = model_dir.join(variant.model_filename());
        let tokenizer_path = model_dir.join(TOKENIZER_FILENAME);

        Ok(Self {
            model_dir,
            variant,
            model: FileFingerprint::from_path(&model_path)?,
            tokenizer: FileFingerprint::from_path(&tokenizer_path)?,
            embed_threads,
        })
    }
}

/// Which ONNX model variant an [`Embedder`] loaded (R-003, T10).
///
/// INT8 is the default CPU path when the quantized artifact is present and
/// loads; FP32 is the always-available fallback with byte-identical numerics to
/// the historical path. The variant feeds [`Embedder::model_id`], which is
/// folded into the embedding `content_key` and `meta.embedding_model` so a
/// variant swap invalidates cached vectors and forces a clean re-embed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelVariant {
    Int8,
    Fp32,
}

impl ModelVariant {
    /// Model-identity string folded into the content-addressed embedding key.
    ///
    /// FP32 keeps the bare [`HF_MODEL_REPO`] identity (so existing FP32 indexes
    /// stay valid); INT8 appends [`MODEL_VARIANT_INT8_SUFFIX`] so its vectors
    /// resolve to distinct keys and never collide with FP32 vectors.
    pub fn model_id(self) -> String {
        match self {
            ModelVariant::Int8 => format!("{HF_MODEL_REPO}{MODEL_VARIANT_INT8_SUFFIX}"),
            ModelVariant::Fp32 => HF_MODEL_REPO.to_string(),
        }
    }

    /// Filename of the ONNX artifact backing this variant.
    fn model_filename(self) -> &'static str {
        match self {
            ModelVariant::Int8 => MODEL_ONNX_INT8_FILENAME,
            ModelVariant::Fp32 => MODEL_FILENAME,
        }
    }
}

/// The variant used when no [`MODEL_VARIANT_ENV_VAR`] override is set (T1).
///
/// INT8 is the v18 established default CPU embedding path. Selection is
/// deterministic: an explicit override wins, otherwise this default is loaded.
/// There is no presence-based auto-selection and no cross-variant fallback — a
/// resolved variant that fails to load surfaces its own error rather than
/// quietly serving the other variant's (numerically different) embeddings.
const DEFAULT_MODEL_VARIANT: ModelVariant = ModelVariant::Int8;

/// Resolves the embedding model variant from the process environment (T1).
///
/// Precedence is explicit [`MODEL_VARIANT_ENV_VAR`] override > [`DEFAULT_MODEL_VARIANT`].
/// An unrecognized override is a hard error propagated at run start — never a
/// degrade through [`EmbeddingUnavailableReason`] and never a silent fallback.
fn resolve_model_variant() -> Result<ModelVariant, OneupError> {
    Ok(parse_model_variant(std::env::var_os(
        MODEL_VARIANT_ENV_VAR,
    ))?)
}

/// Pure parser for the [`MODEL_VARIANT_ENV_VAR`] override value (T1).
///
/// Follows the [`model_downloads_disabled_value`] shape: unset or empty (after
/// trimming) resolves to [`DEFAULT_MODEL_VARIANT`]; `int8`/`fp32`
/// (case-insensitive) select the named variant; any other value is
/// [`EmbeddingError::InvalidVariant`] so a typo aborts the run instead of
/// silently changing (or keeping) the served model.
fn parse_model_variant(value: Option<std::ffi::OsString>) -> Result<ModelVariant, EmbeddingError> {
    let Some(raw) = value else {
        return Ok(DEFAULT_MODEL_VARIANT);
    };
    let text = raw.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_MODEL_VARIANT);
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "int8" => Ok(ModelVariant::Int8),
        "fp32" => Ok(ModelVariant::Fp32),
        _ => Err(EmbeddingError::InvalidVariant(trimmed.to_string())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingUnavailableReason {
    ModelMissing,
    PreviousDownloadFailed,
    ModelDirUnavailable(String),
    LoadFailed(String),
    DownloadFailed(String),
    /// Model state exists on disk but failed verification and could not be
    /// repaired locally; re-indexing re-downloads a verified artifact.
    ArtifactsUnverifiable(String),
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
    pub async fn prepare_for_indexing(
        &mut self,
        embed_threads: usize,
    ) -> Result<EmbeddingLoadStatus, OneupError> {
        self.prepare_for_indexing_with_progress(embed_threads, true)
            .await
    }

    pub async fn prepare_for_indexing_with_progress(
        &mut self,
        embed_threads: usize,
        show_progress_ui: bool,
    ) -> Result<EmbeddingLoadStatus, OneupError> {
        // Resolve the variant first, before touching the filesystem: an invalid
        // override is a hard error at run start (T1), never a degrade through
        // `EmbeddingUnavailableReason` and never a silent cross-variant fallback.
        let variant = resolve_model_variant()?;

        let model_root = match ensure_secure_model_root() {
            Ok(dir) => dir,
            Err(err) => {
                self.cache.clear();
                return Ok(EmbeddingLoadStatus::Unavailable(
                    EmbeddingUnavailableReason::ModelDirUnavailable(err.to_string()),
                ));
            }
        };

        match resolve_model_state(&model_root) {
            Ok(ModelResolution::Active(dir)) => {
                return Ok(self.prepare_from_model_dir(&dir, variant, embed_threads))
            }
            Ok(ModelResolution::Unverifiable(detail)) => {
                // Artifacts are present but failed verification: re-download a
                // verified set instead of silently indexing without embeddings.
                // The download-failure marker only gates downloads for absent
                // models; unverifiable state always warrants a repair attempt.
                tracing::warn!(
                    "model artifacts present but unverifiable ({detail}); re-downloading"
                );
                return Ok(self
                    .prepare_with_download(&model_root, variant, embed_threads, show_progress_ui)
                    .await);
            }
            Ok(ModelResolution::Missing) => {}
            Err(err) => {
                self.cache.clear();
                return Ok(EmbeddingLoadStatus::Unavailable(
                    EmbeddingUnavailableReason::LoadFailed(err.to_string()),
                ));
            }
        }

        if is_download_failed() {
            self.cache.clear();
            return Ok(EmbeddingLoadStatus::Unavailable(
                EmbeddingUnavailableReason::PreviousDownloadFailed,
            ));
        }

        Ok(self
            .prepare_with_download(&model_root, variant, embed_threads, show_progress_ui)
            .await)
    }

    pub fn prepare_for_search(
        &mut self,
        embed_threads: usize,
    ) -> Result<EmbeddingLoadStatus, OneupError> {
        // Same fail-closed variant resolution as the indexing path (T1): a bad
        // override aborts the run rather than degrading query embedding to
        // FTS-only or serving the other variant.
        let variant = resolve_model_variant()?;

        let model_root = match ensure_secure_model_root() {
            Ok(dir) => dir,
            Err(err) => {
                self.cache.clear();
                return Ok(EmbeddingLoadStatus::Unavailable(
                    EmbeddingUnavailableReason::ModelDirUnavailable(err.to_string()),
                ));
            }
        };

        let model_dir = match resolve_model_state(&model_root) {
            Ok(ModelResolution::Active(dir)) => dir,
            Ok(ModelResolution::Unverifiable(detail)) => {
                self.cache.clear();
                return Ok(EmbeddingLoadStatus::Unavailable(
                    EmbeddingUnavailableReason::ArtifactsUnverifiable(detail),
                ));
            }
            Ok(ModelResolution::Missing) => {
                self.cache.clear();
                return Ok(EmbeddingLoadStatus::Unavailable(if is_download_failed() {
                    EmbeddingUnavailableReason::PreviousDownloadFailed
                } else {
                    EmbeddingUnavailableReason::ModelMissing
                }));
            }
            Err(err) => {
                self.cache.clear();
                return Ok(EmbeddingLoadStatus::Unavailable(
                    EmbeddingUnavailableReason::LoadFailed(err.to_string()),
                ));
            }
        };

        Ok(self.prepare_from_model_dir(&model_dir, variant, embed_threads))
    }

    pub fn current_embedder(&mut self) -> Option<&mut Embedder> {
        self.cache.current_mut()
    }

    fn prepare_from_model_dir(
        &mut self,
        model_dir: &Path,
        variant: ModelVariant,
        embed_threads: usize,
    ) -> EmbeddingLoadStatus {
        let key = match EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            model_dir,
            variant,
            embed_threads,
        ) {
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

        match Embedder::from_dir_with_variant(
            &key.model_dir,
            variant,
            embed_threads,
            EMBEDDING_BATCH_SIZE,
        ) {
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
        variant: ModelVariant,
        embed_threads: usize,
        show_progress_ui: bool,
    ) -> EmbeddingLoadStatus {
        match download_and_activate_verified_artifacts(model_root, show_progress_ui).await {
            Ok(model_dir) => {
                clear_download_failure();
                match self.prepare_from_model_dir(&model_dir, variant, embed_threads) {
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
    variant: ModelVariant,
}

/// Reports whether the embedding model files are present on disk.
///
/// Returns `false` if neither an active verified artifact nor a hash-validated
/// legacy flat-file cache is available. Read-only: unlike the prepare paths,
/// this never activates or repairs persisted state.
#[allow(dead_code)]
pub fn is_model_available() -> bool {
    let model_root = match model_dir() {
        Ok(d) => d,
        Err(_) => return false,
    };

    has_active_verified_artifact(&model_root)
        || EXPECTED_ARTIFACT_FILES.iter().all(|artifact| {
            let path = model_root.join(artifact.filename);
            path.exists() && sha256_digest_file(&path).is_ok_and(|digest| digest == artifact.sha256)
        })
}

/// Resolves model availability for status surfaces without initializing an
/// inference session and without writing to the model directory.
///
/// Returns `None` when a verified artifact would resolve, and the unavailable
/// reason otherwise. Status calls stay read-only: this peeks at persisted
/// state and reaches the same availability verdict the prepare paths would,
/// while pointer repair and the one-time legacy import stay gated to the
/// indexing and start paths. Status and search therefore still agree about
/// model availability; self-healing simply happens on the next prepare call.
pub fn model_unavailable_reason_for_status() -> Option<EmbeddingUnavailableReason> {
    let model_root = match model_dir() {
        Ok(dir) => dir,
        Err(err) => {
            return Some(EmbeddingUnavailableReason::ModelDirUnavailable(
                err.to_string(),
            ))
        }
    };

    model_unavailable_reason_for_status_at(&model_root)
}

/// Read-only core of [`model_unavailable_reason_for_status`], split out so
/// tests can pin the no-write guarantee against a temp model root.
fn model_unavailable_reason_for_status_at(model_root: &Path) -> Option<EmbeddingUnavailableReason> {
    match peek_model_state(model_root) {
        Ok(ModelResolution::Active(_)) => None,
        Ok(ModelResolution::Unverifiable(detail)) => {
            Some(EmbeddingUnavailableReason::ArtifactsUnverifiable(detail))
        }
        Ok(ModelResolution::Missing) => Some(if is_download_failed() {
            EmbeddingUnavailableReason::PreviousDownloadFailed
        } else {
            EmbeddingUnavailableReason::ModelMissing
        }),
        Err(err) => Some(EmbeddingUnavailableReason::LoadFailed(err.to_string())),
    }
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

/// Reports whether model auto-download is disabled via
/// [`DISABLE_MODEL_DOWNLOADS_ENV_VAR`].
fn model_downloads_disabled() -> bool {
    model_downloads_disabled_value(std::env::var_os(DISABLE_MODEL_DOWNLOADS_ENV_VAR))
}

fn model_downloads_disabled_value(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|raw| !raw.is_empty() && raw != "0")
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

        let variant = resolve_model_variant()?;
        Self::from_dir_with_variant(&model_dir, variant, intra_threads, batch_size)
    }

    /// Creates an embedder from pre-existing model files at a custom path and
    /// thread count, resolving the variant from the environment (T1).
    #[allow(dead_code)]
    pub fn from_dir_with_threads(dir: &Path, intra_threads: usize) -> Result<Self, OneupError> {
        let variant = resolve_model_variant()?;
        Self::from_dir_with_variant(dir, variant, intra_threads, EMBEDDING_BATCH_SIZE)
    }

    /// Loads the explicitly resolved model variant (T1).
    ///
    /// Selection is deterministic and already decided by `resolve_model_variant`
    /// (override > default `Int8`): this loads exactly `variant` and never probes
    /// for the other artifact or falls back to it. A failed load surfaces its own
    /// error so an unavailable variant degrades to FTS-only (or a hard error for
    /// an invalid override) rather than silently serving the other variant's
    /// numerically different embeddings.
    fn from_dir_with_variant(
        dir: &Path,
        variant: ModelVariant,
        intra_threads: usize,
        batch_size: usize,
    ) -> Result<Self, OneupError> {
        let tokenizer_path = dir.join(TOKENIZER_FILENAME);
        if !tokenizer_path.exists() {
            return Err(EmbeddingError::ModelNotAvailable(format!(
                "tokenizer not found at {}",
                tokenizer_path.display()
            ))
            .into());
        }

        tracing::info!(
            "loading embedding model variant {} ({}) from {}",
            variant.model_id(),
            variant.model_filename(),
            dir.display()
        );

        Self::load_variant(dir, variant, intra_threads, batch_size)
    }

    fn load_variant(
        dir: &Path,
        variant: ModelVariant,
        intra_threads: usize,
        batch_size: usize,
    ) -> Result<Self, OneupError> {
        let model_path = dir.join(variant.model_filename());

        if !model_path.exists() {
            return Err(EmbeddingError::ModelNotAvailable(format!(
                "model not found at {}",
                model_path.display()
            ))
            .into());
        }

        // Pin GraphOptimizationLevel::Level3 explicitly (R-004). It is ort's
        // default today, so this guards against a future or host default that
        // silently lowers graph optimization rather than enabling anything new.
        // Each batch is one serial inference call, so parallelism comes from
        // intra-op threads (`intra_threads`); inter-op is pinned to 1 so
        // node-level parallelism cannot add threads beyond the coordinated
        // intra-op budget (embed_threads + parse jobs <= physical cores).
        // Neither setting changes the produced vectors.
        let session = Session::builder()
            .map_err(|e| EmbeddingError::InferenceFailed(format!("session builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| EmbeddingError::InferenceFailed(format!("set optimization level: {e}")))?
            .with_intra_threads(intra_threads)
            .map_err(|e| EmbeddingError::InferenceFailed(format!("set intra threads: {e}")))?
            .with_inter_threads(1)
            .map_err(|e| EmbeddingError::InferenceFailed(format!("set inter threads: {e}")))?
            .commit_from_file(&model_path)
            .map_err(|e| EmbeddingError::ModelNotAvailable(format!("failed to load model: {e}")))?;

        let mut tokenizer = Tokenizer::from_file(dir.join(TOKENIZER_FILENAME)).map_err(|e| {
            EmbeddingError::TokenizationFailed(format!("failed to load tokenizer: {e}"))
        })?;

        // Programmatically widen the tokenizer window to `EMBEDDING_MAX_TOKENS`
        // (HYP-002). The shipped `tokenizer.json` hard-pins truncation and
        // Fixed padding to 128, so the constant alone is a no-op; overriding
        // here (rather than editing the file) keeps `TOKENIZER_SHA256`
        // unchanged. Existing params are preserved and only the length fields
        // are raised, so inputs of <=128 real tokens still tokenize to
        // byte-identical ids/mask (existing vectors reproduce). Padding must be
        // raised in lockstep with truncation: `run_inference` copies
        // `ids[0..max_len]` uniformly across a mixed-length sub-batch, so a
        // `Fixed(128)` pad width under a 256-token truncation would index past a
        // short row's id buffer and panic.
        let mut truncation = tokenizer.get_truncation().cloned().unwrap_or_default();
        truncation.max_length = EMBEDDING_MAX_TOKENS;
        tokenizer.with_truncation(Some(truncation)).map_err(|e| {
            EmbeddingError::TokenizationFailed(format!("failed to set truncation: {e}"))
        })?;

        let mut padding = tokenizer.get_padding().cloned().unwrap_or_default();
        padding.strategy = tokenizers::PaddingStrategy::Fixed(EMBEDDING_MAX_TOKENS);
        tokenizer.with_padding(Some(padding));

        Ok(Self {
            session,
            tokenizer,
            batch_size,
            variant,
        })
    }

    /// Model-identity string of the loaded variant, folded into the
    /// content-addressed embedding key and `meta.embedding_model` (R-003, T10).
    pub fn model_id(&self) -> String {
        self.variant.model_id()
    }

    /// Which ONNX variant this embedder loaded (INT8 default vs FP32 fallback).
    #[allow(dead_code)]
    pub fn variant(&self) -> ModelVariant {
        self.variant
    }

    /// Embeds a single text, returning a 384-dimensional unit vector.
    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>, OneupError> {
        let results = self.embed_batch(&[text])?;
        Ok(results.into_iter().next().unwrap())
    }

    /// Embeds a batch of texts, returning one 384-dimensional unit vector per input.
    ///
    /// Inputs are length-bucketed before inference (R-005): every input is
    /// tokenized once, the set is sorted by real (un-padded) token length, and
    /// equal-length-ish inputs are grouped into the configured-size sub-batches.
    /// Because the tokenizer pads to a fixed `EMBEDDING_MAX_TOKENS` width but
    /// `run_inference` trims each sub-batch's tensor to its own longest *real*
    /// sequence, grouping short inputs together shrinks that per-sub-batch width
    /// and cuts the padding FLOPs a mixed-length batch would otherwise waste on a
    /// full-width tensor.
    /// Results are scattered back to the caller's original input order via the
    /// sort permutation, so the key->vector contract (`miss_inputs[i]` ->
    /// `vectors[i]`) is preserved. Per-input vectors are byte-identical to the
    /// unbucketed path: mean-pooling masks out pad positions, so the pooled vector
    /// is independent of how many trailing pad tokens (i.e. of the tensor width) a
    /// given input is batched with.
    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, OneupError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut encodings = texts
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

        // Sort input indices by real token length (ascending). A stable sort keeps
        // equal-length inputs in original order; the permutation is inverted below
        // to restore the caller's order, so bucketing never reorders outputs.
        let mut order: Vec<usize> = (0..encodings.len()).collect();
        order.sort_by_key(|&i| real_token_len(&encodings[i]));

        let bucketed: Vec<Encoding> = order
            .iter()
            .map(|&i| std::mem::take(&mut encodings[i]))
            .collect();

        let mut bucketed_embeddings = Vec::with_capacity(bucketed.len());
        for chunk in bucketed.chunks(self.batch_size) {
            bucketed_embeddings.extend(self.run_inference(chunk)?);
        }

        // Scatter each bucketed result back to its original input position.
        let mut all_embeddings: Vec<Vec<f32>> = vec![Vec::new(); bucketed_embeddings.len()];
        for (bucket_pos, &original_pos) in order.iter().enumerate() {
            all_embeddings[original_pos] = std::mem::take(&mut bucketed_embeddings[bucket_pos]);
        }

        Ok(all_embeddings)
    }

    fn run_inference(&mut self, encodings: &[Encoding]) -> Result<Vec<Vec<f32>>, OneupError> {
        let batch_size = encodings.len();

        // Trim the tensor to this sub-batch's longest *real* sequence rather than
        // the tokenizer's fixed `EMBEDDING_MAX_TOKENS` padding. Mean-pooling masks pad
        // positions, so a narrower tensor yields byte-identical vectors while
        // skipping the wasted compute on trailing pads (R-005).
        let max_len = encodings.iter().map(real_token_len).max().unwrap_or(0);

        let mut input_ids = vec![0i64; batch_size * max_len];
        let mut attention_mask = vec![0i64; batch_size * max_len];
        let mut token_type_ids = vec![0i64; batch_size * max_len];

        for (i, enc) in encodings.iter().enumerate() {
            // Copy only the first `max_len` positions: padding is Right, so these
            // are the real tokens for this row and any tokens beyond `max_len`
            // (always pads, since `max_len` is the sub-batch's longest real
            // sequence) are dropped without affecting the masked mean-pool.
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let type_ids = enc.get_type_ids();
            let offset = i * max_len;
            for j in 0..max_len {
                input_ids[offset + j] = ids[j] as i64;
                attention_mask[offset + j] = mask[j] as i64;
                token_type_ids[offset + j] = type_ids[j] as i64;
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

/// Real (un-padded) token count of an encoding: the number of attention-mask
/// positions set to 1. The tokenizer pads to a fixed `EMBEDDING_MAX_TOKENS`
/// width, so `get_ids().len()` is constant; the attention mask is the only
/// signal of how many tokens are real. Used for both length-bucketing and
/// per-sub-batch tensor trimming.
fn real_token_len(enc: &Encoding) -> usize {
    enc.get_attention_mask().iter().filter(|&&m| m == 1).count()
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
    matches!(
        try_load_active_artifact_dir(model_root),
        Ok(ActivePointerState::Active(_))
    )
}

/// Outcome of resolving the embedding model from persisted local state,
/// without downloading.
#[derive(Debug)]
enum ModelResolution {
    /// A verified artifact is active (or was just repaired/activated) and
    /// resolution persisted state so later processes resolve in milliseconds.
    Active(PathBuf),
    /// Model state exists on disk but failed verification. Callers must
    /// re-verify or re-download instead of silently treating the model as
    /// absent: indexing re-downloads, search reports an explicit reason.
    Unverifiable(String),
    /// No model state exists locally.
    Missing,
}

/// State of the persisted active-artifact pointer chain.
enum ActivePointerState {
    Active(PathBuf),
    /// Pointer, manifest, or artifact files exist but the chain is broken.
    Broken(String),
    /// No pointer file exists.
    Missing,
}

/// Outcome of importing legacy flat-file artifacts into the verified store.
enum LegacyActivation {
    Activated(PathBuf),
    /// Legacy files are present (fully or partially) but failed verification.
    Unverifiable(String),
    /// No legacy files exist.
    Absent,
}

/// Resolves the model directory from persisted state, repairing recoverable
/// breakage along the way. Resolution is deterministic and idempotent: every
/// successful path ends with a valid pointer + verified artifact on disk, so
/// indexing and search processes always agree on model availability.
fn resolve_model_state(model_root: &Path) -> Result<ModelResolution, OneupError> {
    let pointer_detail = match try_load_active_artifact_dir(model_root)? {
        ActivePointerState::Active(dir) => return Ok(ModelResolution::Active(dir)),
        ActivePointerState::Broken(detail) => Some(detail),
        ActivePointerState::Missing => None,
    };

    if let Some(repaired_dir) = repair_pointer_from_verified_artifacts(model_root)? {
        return Ok(ModelResolution::Active(repaired_dir));
    }

    match try_activate_legacy_artifacts(model_root)? {
        LegacyActivation::Activated(dir) => Ok(ModelResolution::Active(dir)),
        LegacyActivation::Unverifiable(legacy_detail) => {
            Ok(ModelResolution::Unverifiable(match pointer_detail {
                Some(pointer_detail) => format!("{pointer_detail}; {legacy_detail}"),
                None => legacy_detail,
            }))
        }
        LegacyActivation::Absent => match pointer_detail {
            Some(detail) => Ok(ModelResolution::Unverifiable(detail)),
            None => Ok(ModelResolution::Missing),
        },
    }
}

/// Read-only counterpart of [`resolve_model_state`] for status surfaces:
/// reaches the same availability verdict the prepare paths would, but never
/// writes. Pointer repair and the one-time legacy import (~90MB copy) stay
/// gated to the indexing and start paths, which call
/// [`resolve_model_state`].
fn peek_model_state(model_root: &Path) -> Result<ModelResolution, OneupError> {
    let pointer_detail = match try_load_active_artifact_dir(model_root)? {
        ActivePointerState::Active(dir) => return Ok(ModelResolution::Active(dir)),
        ActivePointerState::Broken(detail) => Some(detail),
        ActivePointerState::Missing => None,
    };

    if let Some((_, artifact_dir)) = find_intact_verified_artifact(model_root) {
        return Ok(ModelResolution::Active(artifact_dir));
    }

    match verify_legacy_artifacts(model_root)? {
        LegacyArtifactState::Verified => Ok(ModelResolution::Active(model_root.to_path_buf())),
        LegacyArtifactState::Unverifiable(legacy_detail) => {
            Ok(ModelResolution::Unverifiable(match pointer_detail {
                Some(pointer_detail) => format!("{pointer_detail}; {legacy_detail}"),
                None => legacy_detail,
            }))
        }
        LegacyArtifactState::Absent => match pointer_detail {
            Some(detail) => Ok(ModelResolution::Unverifiable(detail)),
            None => Ok(ModelResolution::Missing),
        },
    }
}

fn resolve_model_dir_without_download(model_root: &Path) -> Result<Option<PathBuf>, OneupError> {
    Ok(match resolve_model_state(model_root)? {
        ModelResolution::Active(dir) => Some(dir),
        ModelResolution::Unverifiable(_) | ModelResolution::Missing => None,
    })
}

fn try_load_active_artifact_dir(model_root: &Path) -> Result<ActivePointerState, OneupError> {
    let current_path = current_manifest_path(model_root);
    if !current_path.exists() {
        return Ok(ActivePointerState::Missing);
    }
    let current_bytes = match read_validated_file(&current_path, model_root) {
        Ok(bytes) => bytes,
        Err(err) => {
            return Ok(ActivePointerState::Broken(format!(
                "active model pointer is unreadable: {err}"
            )))
        }
    };
    let current: ActiveArtifactPointer =
        match serde_json::from_slice::<ActiveArtifactPointer>(&current_bytes) {
            Ok(pointer) if pointer.is_valid() => pointer,
            _ => {
                return Ok(ActivePointerState::Broken(
                    "active model pointer is invalid".to_string(),
                ))
            }
        };

    let manifest_bytes =
        match read_validated_file(&manifest_path(model_root, &current.artifact_id), model_root) {
            Ok(bytes) => bytes,
            Err(err) => {
                return Ok(ActivePointerState::Broken(format!(
                    "manifest for active model artifact '{}' is unreadable: {err}",
                    current.artifact_id
                )))
            }
        };
    let manifest: VerifiedArtifactManifest =
        match serde_json::from_slice::<VerifiedArtifactManifest>(&manifest_bytes) {
            Ok(manifest)
                if manifest.artifact_id == current.artifact_id && manifest.matches_expected() =>
            {
                manifest
            }
            _ => {
                return Ok(ActivePointerState::Broken(format!(
                    "manifest for active model artifact '{}' does not match the pinned model",
                    current.artifact_id
                )))
            }
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
            return Ok(ActivePointerState::Broken(format!(
                "active model artifact '{}' is missing {}",
                manifest.artifact_id, artifact.filename
            )));
        }
    }

    Ok(ActivePointerState::Active(artifact_dir))
}

/// Re-points `current.json` at an intact verified artifact when the pointer
/// chain is missing or broken. Candidates are digest-verified against the
/// pinned constants before re-pointing, so a successful repair restores the
/// exact same guarantees as a fresh activation. Runs only on broken state;
/// healthy resolution never pays the hashing cost.
fn repair_pointer_from_verified_artifacts(
    model_root: &Path,
) -> Result<Option<PathBuf>, OneupError> {
    match find_intact_verified_artifact(model_root) {
        Some((artifact_id, artifact_dir)) => {
            write_active_artifact_pointer(model_root, &artifact_id)?;
            Ok(Some(artifact_dir))
        }
        None => Ok(None),
    }
}

/// Locates a verified artifact whose manifest matches the pinned model and
/// whose files digest-verify, without touching persisted state.
fn find_intact_verified_artifact(model_root: &Path) -> Option<(String, PathBuf)> {
    let verified_root = verified_dir_path(model_root);
    let entries = fs::read_dir(&verified_root).ok()?;

    for entry in entries.flatten() {
        let artifact_id = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        let artifact_dir = artifact_dir_path(model_root, &artifact_id);

        let manifest_bytes =
            match read_validated_file(&manifest_path(model_root, &artifact_id), model_root) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
        let manifest_ok = serde_json::from_slice::<VerifiedArtifactManifest>(&manifest_bytes)
            .is_ok_and(|manifest| {
                manifest.artifact_id == artifact_id && manifest.matches_expected()
            });
        if !manifest_ok {
            continue;
        }

        let digests_ok = EXPECTED_ARTIFACT_FILES.iter().all(|artifact| {
            sha256_digest_file(&artifact_dir.join(artifact.filename))
                .is_ok_and(|digest| digest == artifact.sha256)
        });
        if !digests_ok {
            continue;
        }

        return Some((artifact_id, artifact_dir));
    }

    None
}

/// Read-only digest verification of the legacy flat-file artifact layout.
enum LegacyArtifactState {
    Verified,
    /// Legacy files are present (fully or partially) but failed verification.
    Unverifiable(String),
    /// No complete legacy file set exists.
    Absent,
}

fn verify_legacy_artifacts(model_root: &Path) -> Result<LegacyArtifactState, OneupError> {
    let legacy_paths: Vec<PathBuf> = EXPECTED_ARTIFACT_FILES
        .iter()
        .map(|artifact| model_root.join(artifact.filename))
        .collect();
    // An incomplete flat-file cache is treated as absent, not unverifiable:
    // missing files are an absence to be downloaded through the marker-gated
    // path, while wrong content on a complete set is a verification failure.
    if legacy_paths.iter().any(|path| !path.exists()) {
        return Ok(LegacyArtifactState::Absent);
    }

    for (artifact, path) in EXPECTED_ARTIFACT_FILES.iter().zip(legacy_paths.iter()) {
        let digest = sha256_digest_file(path)?;
        if digest != artifact.sha256 {
            return Ok(LegacyArtifactState::Unverifiable(format!(
                "legacy {} failed digest verification",
                artifact.label
            )));
        }
    }

    Ok(LegacyArtifactState::Verified)
}

fn try_activate_legacy_artifacts(model_root: &Path) -> Result<LegacyActivation, OneupError> {
    match verify_legacy_artifacts(model_root)? {
        LegacyArtifactState::Verified => {}
        LegacyArtifactState::Unverifiable(detail) => {
            return Ok(LegacyActivation::Unverifiable(detail))
        }
        LegacyArtifactState::Absent => return Ok(LegacyActivation::Absent),
    }

    let legacy_paths: Vec<PathBuf> = EXPECTED_ARTIFACT_FILES
        .iter()
        .map(|artifact| model_root.join(artifact.filename))
        .collect();
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

    copy_result.map(LegacyActivation::Activated)
}

async fn download_and_activate_verified_artifacts(
    model_root: &Path,
    show_progress_ui: bool,
) -> Result<PathBuf, OneupError> {
    if model_downloads_disabled() {
        return Err(EmbeddingError::DownloadFailed(format!(
            "model auto-download disabled via {DISABLE_MODEL_DOWNLOADS_ENV_VAR}"
        ))
        .into());
    }

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

    write_active_artifact_pointer(model_root, artifact_id)?;

    Ok(final_dir)
}

fn write_active_artifact_pointer(model_root: &Path, artifact_id: &str) -> Result<(), OneupError> {
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
    Ok(())
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

/// Test-only guard that pins `ONEUP_MODEL_VARIANT=fp32` and restores the prior
/// value on drop, serialized via `ENV_MUTEX`.
///
/// The FP32 baseline (`model.onnx`) is the always-provisioned artifact, so
/// pinning it keeps variant-agnostic model-gated tests (embedding mechanics,
/// warm-cache reuse, query-embedding stability, pipeline embedding) runnable on
/// any host with the FP32 model present, regardless of whether the INT8 default
/// variant's artifact — provisioned separately (T4) — has been downloaded.
/// Shared across the `embedder`, `pipeline`, and `hybrid` test modules.
#[cfg(test)]
pub(crate) struct Fp32VariantTestGuard {
    _env_lock: std::sync::MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl Fp32VariantTestGuard {
    pub(crate) fn set() -> Self {
        let env_lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prior = std::env::var_os(MODEL_VARIANT_ENV_VAR);
        std::env::set_var(MODEL_VARIANT_ENV_VAR, "fp32");
        Self {
            _env_lock: env_lock,
            prior,
        }
    }
}

#[cfg(test)]
impl Drop for Fp32VariantTestGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(value) => std::env::set_var(MODEL_VARIANT_ENV_VAR, value),
            None => std::env::remove_var(MODEL_VARIANT_ENV_VAR),
        }
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

    fn legacy_label(activation: &LegacyActivation) -> &'static str {
        match activation {
            LegacyActivation::Activated(_) => "Activated",
            LegacyActivation::Unverifiable(_) => "Unverifiable",
            LegacyActivation::Absent => "Absent",
        }
    }

    fn resolution_label(resolution: &ModelResolution) -> &'static str {
        match resolution {
            ModelResolution::Active(_) => "Active",
            ModelResolution::Unverifiable(_) => "Unverifiable",
            ModelResolution::Missing => "Missing",
        }
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
    fn model_downloads_disabled_value_semantics() {
        use std::ffi::OsString;

        assert!(!model_downloads_disabled_value(None));
        assert!(!model_downloads_disabled_value(Some(OsString::from(""))));
        assert!(!model_downloads_disabled_value(Some(OsString::from("0"))));
        assert!(model_downloads_disabled_value(Some(OsString::from("1"))));
        assert!(model_downloads_disabled_value(Some(OsString::from("true"))));
    }

    #[test]
    fn parse_model_variant_semantics() {
        use std::ffi::OsString;

        // Unset and empty/whitespace resolve to the established default (int8),
        // mirroring `model_downloads_disabled_value`'s unset==default shape.
        assert_eq!(parse_model_variant(None).unwrap(), DEFAULT_MODEL_VARIANT);
        assert_eq!(
            parse_model_variant(Some(OsString::from(""))).unwrap(),
            DEFAULT_MODEL_VARIANT
        );
        assert_eq!(
            parse_model_variant(Some(OsString::from("   "))).unwrap(),
            DEFAULT_MODEL_VARIANT
        );

        // Named variants select the corresponding artifact, case-insensitively
        // and tolerant of surrounding whitespace.
        assert_eq!(
            parse_model_variant(Some(OsString::from("int8"))).unwrap(),
            ModelVariant::Int8
        );
        assert_eq!(
            parse_model_variant(Some(OsString::from("fp32"))).unwrap(),
            ModelVariant::Fp32
        );
        assert_eq!(
            parse_model_variant(Some(OsString::from("FP32"))).unwrap(),
            ModelVariant::Fp32
        );
        assert_eq!(
            parse_model_variant(Some(OsString::from("  Int8  "))).unwrap(),
            ModelVariant::Int8
        );

        // Anything else is a hard error carrying the offending value — never a
        // silent fallback to a default or the other variant.
        let err = parse_model_variant(Some(OsString::from("int4"))).unwrap_err();
        assert!(matches!(err, EmbeddingError::InvalidVariant(v) if v == "int4"));
        assert!(parse_model_variant(Some(OsString::from("fp16"))).is_err());
        assert!(parse_model_variant(Some(OsString::from("true"))).is_err());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn invalid_variant_override_propagates_from_prepare_for_indexing() {
        // A bad ONEUP_MODEL_VARIANT must abort at run start as a hard error, not
        // degrade to FTS-only or silently pick a variant (REQ-001 AC3). Resolution
        // happens before any filesystem/model access, so this holds with no model.
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        let _env_lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        std::env::set_var(MODEL_VARIANT_ENV_VAR, "int4");
        let mut runtime = EmbeddingRuntime::default();
        let result = runtime.prepare_for_indexing(1).await;
        std::env::remove_var(MODEL_VARIANT_ENV_VAR);

        let err = result.expect_err("invalid override must be a hard error");
        assert!(
            matches!(
                err,
                OneupError::Embedding(EmbeddingError::InvalidVariant(ref v)) if v == "int4"
            ),
            "expected InvalidVariant, got {err:?}"
        );
    }

    #[test]
    fn model_variant_identity_distinguishes_int8_from_fp32() {
        // R-003 (T10): FP32 keeps the bare repo identity so existing FP32 indexes
        // stay valid; INT8 appends the suffix so its content keys never collide
        // with FP32 vectors. This is the load-bearing correctness point: a variant
        // swap must change the model identity (and therefore every content key),
        // forcing a clean re-embed rather than reusing numerically-different
        // vectors.
        assert_eq!(ModelVariant::Fp32.model_id(), HF_MODEL_REPO);
        assert_ne!(
            ModelVariant::Int8.model_id(),
            ModelVariant::Fp32.model_id(),
            "INT8 and FP32 must resolve to distinct model identities"
        );
        assert!(
            ModelVariant::Int8.model_id().starts_with(HF_MODEL_REPO),
            "INT8 identity is the FP32 repo plus the variant suffix"
        );
        assert_ne!(
            ModelVariant::Int8.model_filename(),
            ModelVariant::Fp32.model_filename(),
            "each variant loads a distinct ONNX artifact"
        );
    }

    /// Restores `HOME`/`XDG_DATA_HOME` on drop — even if an assertion panics — so a
    /// redirected data root never leaks to other tests in this binary.
    struct DataRootGuard {
        home: Option<std::ffi::OsString>,
        xdg_data: Option<std::ffi::OsString>,
    }

    impl Drop for DataRootGuard {
        fn drop(&mut self) {
            fn restore(key: &str, value: Option<std::ffi::OsString>) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            restore("HOME", self.home.take());
            restore("XDG_DATA_HOME", self.xdg_data.take());
        }
    }

    #[test]
    fn mark_and_clear_download_failure() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        // Serialize against every other env-mutating test in this process (e.g. the fs.rs
        // XDG_DATA_HOME tests). dirs::data_dir() honors XDG_DATA_HOME on Linux, so a
        // concurrent env mutation in another module would flip our resolved root between
        // mark/clear/is and break the assertions (the failure this PR's first attempt hit).
        let _env_lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        // mark/clear/is_download_failed operate on the GLOBAL data root
        // (config::data_dir() = dirs::data_dir()/1up). `cargo test` also runs test binaries
        // in parallel and the in-process mutexes above cannot reach a sibling BINARY, so
        // redirect the data root to a process-private temp dir for this test too. The path
        // MUST be canonicalized: secure-fs rejects symlink path components and macOS
        // tempdirs live under /var -> /private/var. The guard restores the environment
        // before the mutexes are released (drop runs in reverse declaration order), so the
        // next serialized embedder test sees the real data root.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let _data_root = DataRootGuard {
            home: std::env::var_os("HOME"),
            xdg_data: std::env::var_os("XDG_DATA_HOME"),
        };
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", home.join("share"));

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

        let key_a = EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            tmp.path(),
            ModelVariant::Fp32,
            1,
        )
        .unwrap();
        let key_b = EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            tmp.path(),
            ModelVariant::Fp32,
            2,
        )
        .unwrap();

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn compatibility_key_changes_when_model_files_change() {
        let tmp = tempfile::tempdir().unwrap();
        write_fake_model_files(tmp.path(), b"model-v1", b"tokenizer-v1");

        let key_before = EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            tmp.path(),
            ModelVariant::Fp32,
            2,
        )
        .unwrap();

        write_fake_model_files(tmp.path(), b"model-v2-with-different-size", b"tokenizer-v1");

        let key_after = EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            tmp.path(),
            ModelVariant::Fp32,
            2,
        )
        .unwrap();
        assert_ne!(key_before, key_after);
    }

    #[test]
    fn compatibility_key_changes_when_variant_changes() {
        // A long-lived daemon must never reuse a warm INT8 embedder for an FP32
        // run (or vice versa): the key folds the resolved variant AND fingerprints
        // that variant's own artifact, so the two resolve to distinct keys (T1).
        let tmp = tempfile::tempdir().unwrap();
        write_fake_model_files(tmp.path(), b"fp32-model", b"tokenizer-v1");
        std::fs::write(tmp.path().join(MODEL_ONNX_INT8_FILENAME), b"int8-model").unwrap();

        let fp32 = EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            tmp.path(),
            ModelVariant::Fp32,
            2,
        )
        .unwrap();
        let int8 = EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            tmp.path(),
            ModelVariant::Int8,
            2,
        )
        .unwrap();

        assert_ne!(fp32, int8);
    }

    #[test]
    fn warm_runtime_reports_only_matching_keys_as_compatible() {
        let tmp = tempfile::tempdir().unwrap();
        write_fake_model_files(tmp.path(), b"model-v1", b"tokenizer-v1");

        let key_a = EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            tmp.path(),
            ModelVariant::Fp32,
            2,
        )
        .unwrap();
        let key_b = EmbeddingCompatibilityKey::from_dir_with_variant_and_threads(
            tmp.path(),
            ModelVariant::Fp32,
            3,
        )
        .unwrap();

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

        let activated = match try_activate_legacy_artifacts(&model_root).unwrap() {
            LegacyActivation::Activated(dir) => dir,
            other => panic!(
                "legacy artifacts should import, got {}",
                legacy_label(&other)
            ),
        };
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

        assert!(
            matches!(result, LegacyActivation::Unverifiable(_)),
            "tampered legacy artifacts must resolve as unverifiable, got {}",
            legacy_label(&result)
        );
        assert_eq!(current.artifact_id, active_id);
    }

    #[test]
    fn legacy_activation_persists_state_for_pointer_based_resolution() {
        // Defect B regression: one successful legacy activation must persist
        // pointer + verified artifact so subsequent processes resolve via that
        // state alone, without the legacy flat files (and without re-hashing
        // them).
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().canonicalize().unwrap().join("models");
        std::fs::create_dir_all(&model_root).unwrap();

        let runtime_dir = runtime_model_dir();
        std::fs::copy(
            runtime_dir.join(MODEL_FILENAME),
            model_root.join(MODEL_FILENAME),
        )
        .unwrap();
        std::fs::copy(
            runtime_dir.join(TOKENIZER_FILENAME),
            model_root.join(TOKENIZER_FILENAME),
        )
        .unwrap();

        let first = resolve_model_dir_without_download(&model_root)
            .unwrap()
            .expect("legacy artifacts should activate");
        assert!(current_manifest_path(&model_root).exists());

        std::fs::remove_file(model_root.join(MODEL_FILENAME)).unwrap();
        std::fs::remove_file(model_root.join(TOKENIZER_FILENAME)).unwrap();

        let second = resolve_model_dir_without_download(&model_root)
            .unwrap()
            .expect("persisted state should keep resolving after legacy files are gone");
        assert_eq!(first, second);
    }

    #[test]
    fn missing_pointer_repairs_from_intact_verified_artifact() {
        // Defect B regression: a deleted/torn pointer with an intact verified
        // artifact must repair and resolve instead of silently reporting no
        // model while verified files sit on disk.
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().canonicalize().unwrap().join("models");
        std::fs::create_dir_all(&model_root).unwrap();

        let runtime_dir = runtime_model_dir();
        std::fs::copy(
            runtime_dir.join(MODEL_FILENAME),
            model_root.join(MODEL_FILENAME),
        )
        .unwrap();
        std::fs::copy(
            runtime_dir.join(TOKENIZER_FILENAME),
            model_root.join(TOKENIZER_FILENAME),
        )
        .unwrap();

        let activated = resolve_model_dir_without_download(&model_root)
            .unwrap()
            .expect("legacy artifacts should activate");
        std::fs::remove_file(model_root.join(MODEL_FILENAME)).unwrap();
        std::fs::remove_file(model_root.join(TOKENIZER_FILENAME)).unwrap();
        std::fs::remove_file(current_manifest_path(&model_root)).unwrap();

        let repaired = resolve_model_dir_without_download(&model_root)
            .unwrap()
            .expect("intact verified artifact should repair the missing pointer");
        assert_eq!(activated, repaired);
        assert!(
            current_manifest_path(&model_root).exists(),
            "repair must persist a fresh pointer"
        );
    }

    #[test]
    fn status_reason_check_does_not_repair_missing_pointer() {
        // Status surfaces must stay read-only: a missing pointer with an
        // intact verified artifact reports the model as available without
        // persisting a repaired pointer (that write belongs to the indexing
        // and start paths).
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().canonicalize().unwrap().join("models");
        std::fs::create_dir_all(&model_root).unwrap();

        let runtime_dir = runtime_model_dir();
        std::fs::copy(
            runtime_dir.join(MODEL_FILENAME),
            model_root.join(MODEL_FILENAME),
        )
        .unwrap();
        std::fs::copy(
            runtime_dir.join(TOKENIZER_FILENAME),
            model_root.join(TOKENIZER_FILENAME),
        )
        .unwrap();
        resolve_model_dir_without_download(&model_root)
            .unwrap()
            .expect("legacy artifacts should activate");
        std::fs::remove_file(model_root.join(MODEL_FILENAME)).unwrap();
        std::fs::remove_file(model_root.join(TOKENIZER_FILENAME)).unwrap();
        std::fs::remove_file(current_manifest_path(&model_root)).unwrap();

        let reason = model_unavailable_reason_for_status_at(&model_root);

        assert!(
            reason.is_none(),
            "an intact verified artifact must report the model as available, got {reason:?}"
        );
        assert!(
            !current_manifest_path(&model_root).exists(),
            "a status call must not persist a repaired pointer"
        );
    }

    #[test]
    fn status_reason_check_does_not_import_legacy_artifacts() {
        // Status surfaces must stay read-only: valid legacy flat files report
        // the model as available without the ~90MB copy into the verified
        // store or any pointer write (those belong to the indexing and start
        // paths).
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().canonicalize().unwrap().join("models");
        std::fs::create_dir_all(&model_root).unwrap();

        let runtime_dir = runtime_model_dir();
        std::fs::copy(
            runtime_dir.join(MODEL_FILENAME),
            model_root.join(MODEL_FILENAME),
        )
        .unwrap();
        std::fs::copy(
            runtime_dir.join(TOKENIZER_FILENAME),
            model_root.join(TOKENIZER_FILENAME),
        )
        .unwrap();

        let reason = model_unavailable_reason_for_status_at(&model_root);

        assert!(
            reason.is_none(),
            "verified legacy artifacts must report the model as available, got {reason:?}"
        );
        assert!(
            !current_manifest_path(&model_root).exists(),
            "a status call must not write an active-artifact pointer"
        );
        assert!(
            !verified_dir_path(&model_root).exists(),
            "a status call must not import legacy artifacts into the verified store"
        );
        assert!(
            !staging_dir_path(&model_root).exists(),
            "a status call must not stage artifact copies"
        );
    }

    #[test]
    fn unverifiable_artifacts_resolve_to_explicit_unverifiable_state() {
        // Defect B regression: present-but-unverifiable artifacts must not be
        // silently reported as missing. Indexing re-downloads on this state
        // and search surfaces an explicit reason.
        let tmp = tempfile::tempdir().unwrap();
        let model_root = tmp.path().canonicalize().unwrap().join("models");
        std::fs::create_dir_all(&model_root).unwrap();

        write_fake_model_files(&model_root, b"tampered-model", b"tampered-tokenizer");
        let tampered = resolve_model_state(&model_root).unwrap();
        assert!(
            matches!(tampered, ModelResolution::Unverifiable(_)),
            "tampered legacy files must resolve as unverifiable, got {}",
            resolution_label(&tampered)
        );

        // An incomplete flat-file cache is an absence (marker-gated download
        // path), not a verification failure.
        std::fs::remove_file(model_root.join(TOKENIZER_FILENAME)).unwrap();
        let partial = resolve_model_state(&model_root).unwrap();
        assert!(
            matches!(partial, ModelResolution::Missing),
            "incomplete legacy files must resolve as missing, got {}",
            resolution_label(&partial)
        );

        std::fs::remove_file(model_root.join(MODEL_FILENAME)).unwrap();
        let absent = resolve_model_state(&model_root).unwrap();
        assert!(
            matches!(absent, ModelResolution::Missing),
            "an empty model root must resolve as missing, got {}",
            resolution_label(&absent)
        );
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
        let _variant = Fp32VariantTestGuard::set();
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let mut runtime = EmbeddingRuntime::default();
        let first = runtime.prepare_for_search(1).unwrap();
        assert!(
            matches!(
                first,
                EmbeddingLoadStatus::Loaded | EmbeddingLoadStatus::Downloaded
            ),
            "expected an initial load, got {first:?}"
        );
        assert!(runtime.current_embedder().is_some());

        let second = runtime.prepare_for_search(1).unwrap();
        assert_eq!(second, EmbeddingLoadStatus::Warm);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn prepare_for_indexing_reuses_warm_runtime_when_model_is_unchanged() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        let _variant = Fp32VariantTestGuard::set();
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let mut runtime = EmbeddingRuntime::default();
        let first = runtime.prepare_for_indexing(1).await.unwrap();
        assert!(
            matches!(
                first,
                EmbeddingLoadStatus::Loaded | EmbeddingLoadStatus::Downloaded
            ),
            "expected an initial load, got {first:?}"
        );
        assert!(runtime.current_embedder().is_some());

        let second = runtime.prepare_for_indexing(1).await.unwrap();
        assert_eq!(second, EmbeddingLoadStatus::Warm);
    }

    #[test]
    fn embed_batch_empty_input() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir_with_variant(
            &runtime_model_dir(),
            ModelVariant::Fp32,
            1,
            EMBEDDING_BATCH_SIZE,
        )
        .unwrap();
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
        let mut embedder = Embedder::from_dir_with_variant(
            &runtime_model_dir(),
            ModelVariant::Fp32,
            1,
            EMBEDDING_BATCH_SIZE,
        )
        .unwrap();
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
        let mut embedder = Embedder::from_dir_with_variant(
            &runtime_model_dir(),
            ModelVariant::Fp32,
            1,
            EMBEDDING_BATCH_SIZE,
        )
        .unwrap();
        let vec = embedder.embed_one("the quick brown fox").unwrap();
        let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "L2 norm should be ~1.0, got {norm}"
        );
    }

    #[test]
    fn tokenizer_window_widens_past_128_and_mixed_batch_is_safe() {
        // T5/REQ-005: the programmatic tokenizer override must widen the
        // effective window past the shipped 128-token pin (a long input yields
        // real_token_len > 128, capped at EMBEDDING_MAX_TOKENS). Before the
        // override this fails — the file-baked truncation caps every encoding
        // at 128. Padding is raised in lockstep with truncation, so a
        // mixed-length sub-batch (a >128-token row next to a short row) must
        // embed without run_inference indexing past the short row's id buffer
        // (HYP-002).
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir_with_variant(
            &runtime_model_dir(),
            ModelVariant::Fp32,
            1,
            EMBEDDING_BATCH_SIZE,
        )
        .unwrap();

        // ~400 space-separated words tokenize well past 256 wordpieces, so the
        // truncation cap (not the input length) determines the token count.
        let long_input = "alpha beta gamma delta epsilon zeta eta theta ".repeat(50);
        let long_enc = embedder
            .tokenizer
            .encode(long_input.as_str(), true)
            .unwrap();
        let long_len = real_token_len(&long_enc);
        assert!(
            long_len > 128,
            "override must widen the window past the shipped 128-token pin, got {long_len}"
        );
        assert!(
            long_len <= EMBEDDING_MAX_TOKENS,
            "the window must stay capped at EMBEDDING_MAX_TOKENS ({EMBEDDING_MAX_TOKENS}), got {long_len}"
        );

        // Mixed-length batch: a long (>128-token) input alongside a short one
        // must embed without panicking and yield one unit vector per input.
        let results = embedder
            .embed_batch(&[long_input.as_str(), "short"])
            .unwrap();
        assert_eq!(results.len(), 2);
        for vec in &results {
            assert_eq!(vec.len(), EMBEDDING_DIM);
        }
    }

    #[test]
    fn embed_batch_multiple_texts() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir_with_variant(
            &runtime_model_dir(),
            ModelVariant::Fp32,
            1,
            EMBEDDING_BATCH_SIZE,
        )
        .unwrap();
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
        let mut embedder = Embedder::from_dir_with_variant(
            &runtime_model_dir(),
            ModelVariant::Fp32,
            1,
            EMBEDDING_BATCH_SIZE,
        )
        .unwrap();
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
            variant: ModelVariant::Fp32,
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

    #[test]
    fn length_bucketed_batch_preserves_per_input_vectors_and_order() {
        let _lock = MODEL_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let dir = runtime_model_dir();
        // batch_size 2 forces multiple sub-batches, so length-bucketing must
        // group across the original order and then restore it.
        let mut embedder = Embedder {
            session: Session::builder()
                .unwrap()
                .with_intra_threads(1)
                .unwrap()
                .commit_from_file(dir.join(MODEL_FILENAME))
                .unwrap(),
            tokenizer: Tokenizer::from_file(dir.join(TOKENIZER_FILENAME)).unwrap(),
            batch_size: 2,
            variant: ModelVariant::Fp32,
        };

        // Deliberately interleaved short/long inputs: a stable sort by token
        // length reorders these into a different bucket order than the input
        // order, so a missing or wrong inverse permutation flips the mapping.
        let long = "error handling and propagation across asynchronous tokio tasks \
            with cancellation tokens and structured concurrency in a long rust function body"
            .to_string();
        let texts: Vec<&str> = vec![
            "a",
            long.as_str(),
            "bb",
            "concurrent vector search over an embedding index",
            "c",
        ];

        let bucketed = embedder.embed_batch(&texts).unwrap();
        assert_eq!(bucketed.len(), texts.len());

        // Ground truth: each input embedded on its own (order-independent). The
        // bucketed result must equal it position-for-position and bit-for-bit.
        for (i, &text) in texts.iter().enumerate() {
            let standalone = embedder.embed_one(text).unwrap();
            assert_eq!(
                bucketed[i], standalone,
                "bucketed vector for input {i} ({text:?}) must be byte-identical to the standalone embedding"
            );
        }
    }
}
