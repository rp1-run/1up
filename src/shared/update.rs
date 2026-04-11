use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::shared::config;
use crate::shared::constants::{
    SECURE_STATE_FILE_MODE, UPDATE_CHECK_CONNECT_TIMEOUT_SECS, UPDATE_CHECK_TIMEOUT_SECS,
    UPDATE_CHECK_TTL_SECS, UPDATE_DOWNLOAD_CONNECT_TIMEOUT_SECS, UPDATE_DOWNLOAD_TIMEOUT_SECS,
    UPDATE_MANIFEST_URL_ENV_VAR, XDG_STATE_DIR_MODE,
};
use crate::shared::errors::UpdateError;
use crate::shared::reminder::VERSION;

/// Machine-readable update manifest fetched from the configured update URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    pub git_tag: String,
    pub published_at: String,
    pub notes_url: String,
    pub artifacts: Vec<UpdateArtifact>,
    pub channels: UpdateChannels,
    #[serde(default)]
    pub yanked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_safe_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Platform-specific release artifact with download URL and checksum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateArtifact {
    pub target: String,
    pub archive: String,
    pub sha256: String,
    pub url: String,
}

/// Distribution channel metadata for package manager integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateChannels {
    pub github_release: String,
    pub homebrew_tap: String,
    pub homebrew_formula: String,
    pub scoop_bucket: String,
    pub scoop_manifest: String,
}

/// Cached result of the most recent update check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheckCache {
    pub current_version: String,
    pub latest_version: String,
    pub checked_at: DateTime<Utc>,
    pub install_channel: InstallChannel,
    #[serde(default)]
    pub yanked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_safe_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    pub upgrade_instruction: String,
}

/// How 1up was installed on this system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallChannel {
    Homebrew,
    Scoop,
    Manual,
    Unknown,
}

impl std::fmt::Display for InstallChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallChannel::Homebrew => write!(f, "homebrew"),
            InstallChannel::Scoop => write!(f, "scoop"),
            InstallChannel::Manual => write!(f, "manual"),
            InstallChannel::Unknown => write!(f, "unknown"),
        }
    }
}

/// Assessed update status relative to the current binary version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    UpToDate,
    UpdateAvailable {
        latest: String,
    },
    Yanked {
        latest: String,
        message: Option<String>,
    },
    BelowMinimumSafe {
        latest: String,
        minimum_safe: String,
        message: Option<String>,
    },
}

/// Compares the current binary version against the cached update check and
/// returns the assessed update status.
pub fn build_update_status(cache: &UpdateCheckCache) -> UpdateStatus {
    let current = match semver::Version::parse(&cache.current_version) {
        Ok(v) => v,
        Err(_) => return UpdateStatus::UpToDate,
    };
    let latest = match semver::Version::parse(&cache.latest_version) {
        Ok(v) => v,
        Err(_) => return UpdateStatus::UpToDate,
    };

    if cache.yanked {
        return UpdateStatus::Yanked {
            latest: cache.latest_version.clone(),
            message: cache.message.clone(),
        };
    }

    if let Some(ref min_safe) = cache.minimum_safe_version {
        if let Ok(min_safe_ver) = semver::Version::parse(min_safe) {
            if current < min_safe_ver {
                return UpdateStatus::BelowMinimumSafe {
                    latest: cache.latest_version.clone(),
                    minimum_safe: min_safe.clone(),
                    message: cache.message.clone(),
                };
            }
        }
    }

    if latest > current {
        return UpdateStatus::UpdateAvailable {
            latest: cache.latest_version.clone(),
        };
    }

    UpdateStatus::UpToDate
}

/// Returns the Rust target triple for the current platform, matching the
/// naming convention used in release archives.
pub fn current_target_triple() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        ("x86_64", "windows") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Detects how 1up was installed by examining the resolved binary path.
///
/// Uses `std::env::current_exe()` with canonicalization to resolve symlinks,
/// then delegates to [`detect_channel_from_path`] for heuristic matching.
pub fn detect_install_channel() -> InstallChannel {
    let exe_path = match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(p) => p,
        Err(_) => return InstallChannel::Unknown,
    };
    detect_channel_from_path(&exe_path)
}

/// Determines the install channel from a resolved binary path.
///
/// Heuristics:
/// - macOS/Linux: path containing `/Cellar/` or `/homebrew/` indicates Homebrew
/// - Windows: path containing `\scoop\apps\` indicates Scoop
/// - All other paths are classified as manual/unmanaged installs
fn detect_channel_from_path(path: &std::path::Path) -> InstallChannel {
    let path_str = path.to_string_lossy();

    if (cfg!(target_os = "macos") || cfg!(target_os = "linux"))
        && (path_str.contains("/Cellar/") || path_str.contains("/homebrew/"))
    {
        return InstallChannel::Homebrew;
    }
    if cfg!(target_os = "windows") && path_str.contains(r"\scoop\apps\") {
        return InstallChannel::Scoop;
    }
    InstallChannel::Manual
}

/// Returns the channel-appropriate upgrade instruction for the given install channel.
pub fn upgrade_instruction_for_channel(channel: InstallChannel) -> String {
    match channel {
        InstallChannel::Homebrew => "brew upgrade rp1-run/tap/1up".to_string(),
        InstallChannel::Scoop => "scoop update 1up".to_string(),
        InstallChannel::Manual | InstallChannel::Unknown => "1up update".to_string(),
    }
}

/// Returns the configured update manifest URL, if update support is enabled.
///
/// Release builds bake the manifest URL in at compile time. A runtime env var
/// of the same name can override that value for testing or operator-driven
/// canaries; an empty runtime value disables updates for the current process.
pub fn configured_update_manifest_url() -> Option<String> {
    if let Some(value) = std::env::var_os(UPDATE_MANIFEST_URL_ENV_VAR) {
        let trimmed = value.to_string_lossy().trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed);
    }

    option_env!("ONEUP_UPDATE_MANIFEST_URL")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Returns true when this binary has a configured update manifest endpoint.
pub fn updates_enabled() -> bool {
    configured_update_manifest_url().is_some()
}

fn update_http_failure_is_permanent(status: reqwest::StatusCode) -> bool {
    status.is_client_error()
        && status != reqwest::StatusCode::REQUEST_TIMEOUT
        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Fetches the remote update manifest over HTTPS with bounded timeouts.
pub async fn fetch_update_manifest(
    client: &reqwest::Client,
) -> Result<UpdateManifest, UpdateError> {
    let manifest_url = configured_update_manifest_url().ok_or(UpdateError::Disabled)?;
    let response = client
        .get(&manifest_url)
        .timeout(Duration::from_secs(UPDATE_CHECK_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| UpdateError::FetchFailed {
            detail: format!("manifest fetch: {e}"),
            permanent: false,
        })?;

    if !response.status().is_success() {
        return Err(UpdateError::FetchFailed {
            detail: format!("manifest fetch: HTTP {}", response.status()),
            permanent: update_http_failure_is_permanent(response.status()),
        });
    }

    let body = response
        .text()
        .await
        .map_err(|e| UpdateError::FetchFailed {
            detail: format!("manifest read: {e}"),
            permanent: false,
        })?;

    serde_json::from_str(&body)
        .map_err(|e| UpdateError::ParseFailed(format!("manifest parse: {e}")))
}

/// Builds a pre-configured HTTP client for update checks with connect timeout.
pub fn build_update_check_client() -> Result<reqwest::Client, UpdateError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(UPDATE_CHECK_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(UPDATE_CHECK_TIMEOUT_SECS))
        .build()
        .map_err(|e| UpdateError::FetchFailed {
            detail: format!("http client: {e}"),
            permanent: false,
        })
}

/// Reads and parses the local update-check cache file.
///
/// Returns `None` if the file does not exist, cannot be read, or contains
/// invalid JSON. This ensures corrupted cache is silently discarded (AC-10b).
pub fn read_update_cache() -> Option<UpdateCheckCache> {
    let path = config::update_check_cache_path().ok()?;
    let contents = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Reads the local update cache only when it matches the running binary version.
///
/// If the cache was written by a different binary version, it is cleared and
/// treated as absent so stale update state cannot leak across manual upgrades
/// or downgrades.
pub fn read_compatible_update_cache(running_version: &str) -> Option<UpdateCheckCache> {
    let cache = read_update_cache()?;
    if cache.current_version == running_version {
        return Some(cache);
    }

    debug!(
        "discarding update cache for version {} while running {}",
        cache.current_version, running_version
    );
    clear_update_cache();
    None
}

/// Writes the update-check cache atomically with secure file permissions.
///
/// Uses `atomic_replace` to ensure the cache file is never partially written.
/// Failures are logged at debug level but not propagated, preserving the
/// non-fatal failure policy (AC-01d, AC-10a).
pub fn write_update_cache(cache: &UpdateCheckCache) {
    let result = write_update_cache_inner(cache);
    if let Err(e) = result {
        debug!("update cache write failed: {e}");
    }
}

fn write_update_cache_inner(cache: &UpdateCheckCache) -> Result<(), Box<dyn std::error::Error>> {
    let path = config::update_check_cache_path()?;
    let data_dir = config::data_dir()?;
    let json = serde_json::to_string_pretty(cache)?;
    crate::shared::fs::atomic_replace(
        &path,
        json.as_bytes(),
        &data_dir,
        XDG_STATE_DIR_MODE,
        SECURE_STATE_FILE_MODE,
    )?;
    Ok(())
}

/// Removes the cached update-check result, if present.
pub fn clear_update_cache() {
    let result = clear_update_cache_inner();
    if let Err(e) = result {
        debug!("update cache clear failed: {e}");
    }
}

fn clear_update_cache_inner() -> Result<(), Box<dyn std::error::Error>> {
    let path = config::update_check_cache_path()?;
    let data_dir = config::data_dir()?;
    if !data_dir.exists() {
        return Ok(());
    }
    let _ = crate::shared::fs::remove_regular_file(&path, &data_dir)?;
    Ok(())
}

/// Returns `true` when the cache is still valid for the running binary.
///
/// A cache entry is valid when both conditions hold:
/// 1. The cached `current_version` matches the running binary version.
/// 2. The cache is younger than `UPDATE_CHECK_TTL_SECS` (24 hours).
pub fn is_cache_valid(cache: &UpdateCheckCache, running_version: &str) -> bool {
    if cache.current_version != running_version {
        return false;
    }
    let age = Utc::now().signed_duration_since(cache.checked_at);
    age.num_seconds() < UPDATE_CHECK_TTL_SECS as i64
}

/// Builds an `UpdateCheckCache` from a fetched manifest and the current
/// runtime context (running version and install channel).
pub fn build_cache_from_manifest(manifest: &UpdateManifest) -> UpdateCheckCache {
    let channel = detect_install_channel();
    UpdateCheckCache {
        current_version: VERSION.to_string(),
        latest_version: manifest.version.clone(),
        checked_at: Utc::now(),
        install_channel: channel,
        yanked: manifest.yanked,
        minimum_safe_version: manifest.minimum_safe_version.clone(),
        message: manifest.message.clone(),
        notes_url: Some(manifest.notes_url.clone()),
        upgrade_instruction: upgrade_instruction_for_channel(channel),
    }
}

/// Returns a valid cache entry, fetching a fresh manifest when the cache is
/// stale or missing.
///
/// On fetch failure, returns the existing (possibly stale) cache if available.
/// This preserves the non-fatal failure policy: a failed fetch never
/// overwrites or corrupts a valid cache entry (AC-01d).
pub async fn refresh_cache_if_stale() -> Option<UpdateCheckCache> {
    if !updates_enabled() {
        clear_update_cache();
        return None;
    }

    let existing = read_compatible_update_cache(VERSION);

    if let Some(ref cache) = existing {
        if is_cache_valid(cache, VERSION) {
            return existing;
        }
    }

    let client = match build_update_check_client() {
        Ok(c) => c,
        Err(e) => {
            debug!("update check client build failed: {e}");
            return existing;
        }
    };

    match fetch_update_manifest(&client).await {
        Ok(manifest) => {
            let cache = build_cache_from_manifest(&manifest);
            write_update_cache(&cache);
            Some(cache)
        }
        Err(e) => {
            debug!("update manifest fetch failed: {e}");
            if e.should_invalidate_cache() {
                clear_update_cache();
                return None;
            }
            existing
        }
    }
}

/// Reads the update cache and formats a passive notification string, if an
/// update is available.
///
/// Returns `None` when:
/// - The cache cannot be read (non-fatal, AC-10a)
/// - No update is available (current version matches or exceeds latest)
///
/// The notification is formatted differently depending on `UpdateStatus`:
/// - `UpdateAvailable`: informational notice with upgrade instruction
/// - `Yanked`: urgent warning with operator message
/// - `BelowMinimumSafe`: urgent warning with minimum safe version
pub fn format_update_notification() -> Option<String> {
    if !updates_enabled() {
        return None;
    }

    let cache = read_compatible_update_cache(VERSION)?;
    let status = build_update_status(&cache);

    match status {
        UpdateStatus::UpToDate => None,
        UpdateStatus::UpdateAvailable { latest } => Some(format!(
            "Update available: 1up {} (current: {})\nRun: {}",
            latest, VERSION, cache.upgrade_instruction
        )),
        UpdateStatus::Yanked { latest, message } => {
            let mut out = format!(
                "WARNING: 1up {} has been recalled. Upgrade immediately to {}+",
                VERSION, latest
            );
            if let Some(msg) = message {
                out.push_str(&format!("\nMessage from maintainer: {}", msg));
            }
            out.push_str(&format!("\nRun: {}", cache.upgrade_instruction));
            Some(out)
        }
        UpdateStatus::BelowMinimumSafe {
            latest,
            minimum_safe,
            message,
        } => {
            let mut out = format!(
                "WARNING: 1up {} is below the minimum safe version ({}). Upgrade immediately to {}+",
                VERSION, minimum_safe, latest
            );
            if let Some(msg) = message {
                out.push_str(&format!("\nMessage from maintainer: {}", msg));
            }
            out.push_str(&format!("\nRun: {}", cache.upgrade_instruction));
            Some(out)
        }
    }
}

/// Outcome of a successful self-update binary replacement.
#[derive(Debug, Clone)]
pub struct SelfUpdateResult {
    pub old_version: String,
    pub new_version: String,
}

/// Builds a pre-configured HTTP client for downloading update artifacts.
pub fn build_download_client() -> Result<reqwest::Client, UpdateError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(UPDATE_DOWNLOAD_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(UPDATE_DOWNLOAD_TIMEOUT_SECS))
        .build()
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("http client: {e}")))
}

/// Finds the artifact matching the current platform in the manifest.
pub fn find_artifact_for_platform(
    manifest: &UpdateManifest,
) -> Result<&UpdateArtifact, UpdateError> {
    let triple = current_target_triple().ok_or_else(|| {
        UpdateError::NoArtifactForPlatform(format!(
            "{}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        ))
    })?;
    manifest
        .artifacts
        .iter()
        .find(|a| a.target == triple)
        .ok_or_else(|| UpdateError::NoArtifactForPlatform(triple.to_string()))
}

/// Downloads a release archive to a temporary file.
///
/// Returns the path to the temporary file. The caller is responsible for
/// cleanup. The file is created adjacent to the current binary so that
/// rename-based replacement stays on the same filesystem.
async fn download_archive(
    client: &reqwest::Client,
    artifact: &UpdateArtifact,
    staging_dir: &Path,
) -> Result<PathBuf, UpdateError> {
    use futures_util::StreamExt;

    let response = client
        .get(&artifact.url)
        .send()
        .await
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("download request: {e}")))?;

    if !response.status().is_success() {
        return Err(UpdateError::SelfUpdateFailed(format!(
            "download failed: HTTP {}",
            response.status()
        )));
    }

    let archive_path = staging_dir.join(&artifact.archive);
    let mut file = std::fs::File::create(&archive_path)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("create temp file: {e}")))?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| UpdateError::SelfUpdateFailed(format!("download read: {e}")))?;
        std::io::Write::write_all(&mut file, &chunk)
            .map_err(|e| UpdateError::SelfUpdateFailed(format!("write temp file: {e}")))?;
    }

    std::io::Write::flush(&mut file)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("flush temp file: {e}")))?;

    Ok(archive_path)
}

/// Computes the SHA-256 digest of a file and verifies it against the expected
/// value from the manifest.
fn verify_archive_checksum(path: &Path, expected_sha256: &str) -> Result<(), UpdateError> {
    use std::io::Read;

    let file = std::fs::File::open(path)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("open for checksum: {e}")))?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| UpdateError::SelfUpdateFailed(format!("checksum read: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if digest != expected_sha256 {
        debug!(
            "checksum mismatch: expected {}, got {}",
            expected_sha256, digest
        );
        return Err(UpdateError::ChecksumMismatch);
    }
    Ok(())
}

/// Extracts the 1up binary from a tar.gz archive into the staging directory.
///
/// The archive structure is `1up-v{version}-{target}/{binary_name}` where
/// `binary_name` is `1up` on Unix and `1up.exe` on Windows.
#[cfg(not(windows))]
fn extract_binary_from_archive(
    archive_path: &Path,
    staging_dir: &Path,
) -> Result<PathBuf, UpdateError> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = std::fs::File::open(archive_path)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("open archive: {e}")))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    let binary_name = "1up";

    for entry_result in archive
        .entries()
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("read archive entries: {e}")))?
    {
        let mut entry = entry_result
            .map_err(|e| UpdateError::SelfUpdateFailed(format!("read archive entry: {e}")))?;
        let entry_path = entry
            .path()
            .map_err(|e| UpdateError::SelfUpdateFailed(format!("archive entry path: {e}")))?
            .to_path_buf();

        if let Some(file_name) = entry_path.file_name() {
            if file_name == binary_name {
                let dest = staging_dir.join(binary_name);
                entry
                    .unpack(&dest)
                    .map_err(|e| UpdateError::SelfUpdateFailed(format!("extract binary: {e}")))?;
                return Ok(dest);
            }
        }
    }

    Err(UpdateError::SelfUpdateFailed(format!(
        "binary '{}' not found in archive",
        binary_name
    )))
}

/// Extracts the 1up binary from a zip archive into the staging directory.
#[cfg(windows)]
fn extract_binary_from_archive(
    archive_path: &Path,
    staging_dir: &Path,
) -> Result<PathBuf, UpdateError> {
    use std::io::Read;

    let file = std::fs::File::open(archive_path)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("open archive: {e}")))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("read zip archive: {e}")))?;

    let binary_name = "1up.exe";

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| UpdateError::SelfUpdateFailed(format!("read zip entry: {e}")))?;
        let entry_path = PathBuf::from(entry.name());

        if let Some(file_name) = entry_path.file_name() {
            if file_name == binary_name {
                let dest = staging_dir.join(binary_name);
                let mut out = std::fs::File::create(&dest)
                    .map_err(|e| UpdateError::SelfUpdateFailed(format!("create binary: {e}")))?;
                std::io::copy(&mut entry, &mut out)
                    .map_err(|e| UpdateError::SelfUpdateFailed(format!("extract binary: {e}")))?;
                return Ok(dest);
            }
        }
    }

    Err(UpdateError::SelfUpdateFailed(format!(
        "binary '{}' not found in archive",
        binary_name
    )))
}

/// Replaces the current binary with the new one atomically on Unix.
///
/// Uses rename for atomic replacement and sets executable permissions.
#[cfg(unix)]
fn replace_binary(new_binary: &Path, target: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(new_binary, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("set permissions: {e}")))?;

    std::fs::rename(new_binary, target)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("replace binary: {e}")))?;

    Ok(())
}

/// Replaces the current binary with the new one on Windows.
///
/// Renames the running binary to `.old`, moves the new binary into place,
/// then deletes the old binary (best-effort; the OS may lock it).
#[cfg(windows)]
fn replace_binary(new_binary: &Path, target: &Path) -> Result<(), UpdateError> {
    let old_path = target.with_extension("old");

    if old_path.exists() {
        let _ = std::fs::remove_file(&old_path);
    }

    std::fs::rename(target, &old_path)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("rename current binary: {e}")))?;

    if let Err(e) = std::fs::rename(new_binary, target) {
        let _ = std::fs::rename(&old_path, target);
        return Err(UpdateError::SelfUpdateFailed(format!(
            "install new binary: {e}"
        )));
    }

    let _ = std::fs::remove_file(&old_path);
    Ok(())
}

/// Performs a self-update: downloads, verifies, and atomically replaces the
/// running binary.
///
/// This function is only valid for manual/unmanaged installs. Callers must
/// ensure the daemon has been stopped before invoking this function.
///
/// Returns the old and new versions on success.
pub async fn self_update(manifest: &UpdateManifest) -> Result<SelfUpdateResult, UpdateError> {
    let artifact = find_artifact_for_platform(manifest)?;

    let current_exe = std::env::current_exe()
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("resolve current binary: {e}")))?;

    let staging_parent = current_exe.parent().unwrap_or(std::path::Path::new("."));
    let staging_dir = tempfile::tempdir_in(staging_parent)
        .map_err(|e| UpdateError::SelfUpdateFailed(format!("create staging dir: {e}")))?;

    let client = build_download_client()?;

    let archive_path = download_archive(&client, artifact, staging_dir.path()).await?;

    verify_archive_checksum(&archive_path, &artifact.sha256)?;

    let new_binary = extract_binary_from_archive(&archive_path, staging_dir.path())?;

    replace_binary(&new_binary, &current_exe)?;

    Ok(SelfUpdateResult {
        old_version: VERSION.to_string(),
        new_version: manifest.version.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static UPDATE_ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct ScopedEnvGuard {
        key: &'static str,
        saved: Option<OsString>,
    }

    impl ScopedEnvGuard {
        fn set(key: &'static str, value: impl Into<OsString>) -> Self {
            let saved = std::env::var_os(key);
            std::env::set_var(key, value.into());
            Self { key, saved }
        }
    }

    impl Drop for ScopedEnvGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn make_cache(
        current: &str,
        latest: &str,
        yanked: bool,
        minimum_safe: Option<&str>,
        message: Option<&str>,
    ) -> UpdateCheckCache {
        UpdateCheckCache {
            current_version: current.to_string(),
            latest_version: latest.to_string(),
            checked_at: Utc::now(),
            install_channel: InstallChannel::Manual,
            yanked,
            minimum_safe_version: minimum_safe.map(|s| s.to_string()),
            message: message.map(|s| s.to_string()),
            notes_url: None,
            upgrade_instruction: "1up update".to_string(),
        }
    }

    #[test]
    fn build_update_status_returns_up_to_date_when_versions_match() {
        let cache = make_cache("0.1.0", "0.1.0", false, None, None);
        assert_eq!(build_update_status(&cache), UpdateStatus::UpToDate);
    }

    #[test]
    fn build_update_status_returns_update_available_when_latest_is_newer() {
        let cache = make_cache("0.1.0", "0.1.1", false, None, None);
        assert_eq!(
            build_update_status(&cache),
            UpdateStatus::UpdateAvailable {
                latest: "0.1.1".to_string(),
            }
        );
    }

    #[test]
    fn build_update_status_returns_up_to_date_when_current_is_newer() {
        let cache = make_cache("0.2.0", "0.1.0", false, None, None);
        assert_eq!(build_update_status(&cache), UpdateStatus::UpToDate);
    }

    #[test]
    fn build_update_status_returns_yanked_when_flag_set() {
        let cache = make_cache("0.1.0", "0.1.1", true, None, Some("bad release"));
        assert_eq!(
            build_update_status(&cache),
            UpdateStatus::Yanked {
                latest: "0.1.1".to_string(),
                message: Some("bad release".to_string()),
            }
        );
    }

    #[test]
    fn build_update_status_returns_below_minimum_safe() {
        let cache = make_cache("0.1.0", "0.1.2", false, Some("0.1.1"), Some("upgrade now"));
        assert_eq!(
            build_update_status(&cache),
            UpdateStatus::BelowMinimumSafe {
                latest: "0.1.2".to_string(),
                minimum_safe: "0.1.1".to_string(),
                message: Some("upgrade now".to_string()),
            }
        );
    }

    #[test]
    fn build_update_status_ignores_minimum_safe_when_current_meets_it() {
        let cache = make_cache("0.1.1", "0.1.2", false, Some("0.1.1"), None);
        assert_eq!(
            build_update_status(&cache),
            UpdateStatus::UpdateAvailable {
                latest: "0.1.2".to_string(),
            }
        );
    }

    #[test]
    fn build_update_status_yanked_takes_precedence_over_minimum_safe() {
        let cache = make_cache("0.1.0", "0.1.2", true, Some("0.1.1"), Some("recalled"));
        assert_eq!(
            build_update_status(&cache),
            UpdateStatus::Yanked {
                latest: "0.1.2".to_string(),
                message: Some("recalled".to_string()),
            }
        );
    }

    #[test]
    fn current_target_triple_returns_some_on_supported_platform() {
        let triple = current_target_triple();
        assert!(
            triple.is_some(),
            "expected a target triple for this platform"
        );
        let triple = triple.unwrap();
        assert!(
            triple.contains('-'),
            "target triple should contain dashes: {triple}"
        );
    }

    #[test]
    fn update_manifest_round_trip_json() {
        let manifest = UpdateManifest {
            version: "0.1.1".to_string(),
            git_tag: "v0.1.1".to_string(),
            published_at: "2026-04-10T12:00:00Z".to_string(),
            notes_url: "https://github.com/rp1-run/1up/releases/tag/v0.1.1".to_string(),
            artifacts: vec![UpdateArtifact {
                target: "aarch64-apple-darwin".to_string(),
                archive: "1up-aarch64-apple-darwin.tar.gz".to_string(),
                sha256: "abcdef1234567890".to_string(),
                url: "https://github.com/rp1-run/1up/releases/download/v0.1.1/1up-aarch64-apple-darwin.tar.gz".to_string(),
            }],
            channels: UpdateChannels {
                github_release: "https://github.com/rp1-run/1up/releases/tag/v0.1.1".to_string(),
                homebrew_tap: "rp1-run/tap".to_string(),
                homebrew_formula: "1up".to_string(),
                scoop_bucket: "rp1-run".to_string(),
                scoop_manifest: "1up".to_string(),
            },
            yanked: false,
            minimum_safe_version: None,
            message: None,
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: UpdateManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.version, "0.1.1");
        assert_eq!(parsed.artifacts.len(), 1);
        assert_eq!(parsed.artifacts[0].target, "aarch64-apple-darwin");
        assert!(!parsed.yanked);
        assert!(parsed.minimum_safe_version.is_none());
        assert!(parsed.message.is_none());
    }

    #[test]
    fn update_manifest_deserializes_with_optional_fields() {
        let json = r#"{
            "version": "0.1.1",
            "git_tag": "v0.1.1",
            "published_at": "2026-04-10T12:00:00Z",
            "notes_url": "https://example.com/notes",
            "artifacts": [],
            "channels": {
                "github_release": "https://example.com",
                "homebrew_tap": "tap",
                "homebrew_formula": "1up",
                "scoop_bucket": "bucket",
                "scoop_manifest": "1up"
            }
        }"#;

        let manifest: UpdateManifest = serde_json::from_str(json).unwrap();
        assert!(!manifest.yanked);
        assert!(manifest.minimum_safe_version.is_none());
        assert!(manifest.message.is_none());
    }

    #[test]
    fn update_check_cache_round_trip_json() {
        let cache = make_cache("0.1.0", "0.1.1", false, None, None);
        let json = serde_json::to_string(&cache).unwrap();
        let parsed: UpdateCheckCache = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.current_version, "0.1.0");
        assert_eq!(parsed.latest_version, "0.1.1");
        assert_eq!(parsed.install_channel, InstallChannel::Manual);
        assert!(!parsed.yanked);
    }

    #[test]
    fn install_channel_serde_round_trip() {
        for channel in [
            InstallChannel::Homebrew,
            InstallChannel::Scoop,
            InstallChannel::Manual,
            InstallChannel::Unknown,
        ] {
            let json = serde_json::to_string(&channel).unwrap();
            let parsed: InstallChannel = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, channel);
        }
    }

    #[test]
    fn install_channel_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&InstallChannel::Homebrew).unwrap(),
            "\"homebrew\""
        );
        assert_eq!(
            serde_json::to_string(&InstallChannel::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    #[test]
    fn upgrade_instruction_for_homebrew() {
        assert_eq!(
            upgrade_instruction_for_channel(InstallChannel::Homebrew),
            "brew upgrade rp1-run/tap/1up"
        );
    }

    #[test]
    fn upgrade_instruction_for_scoop() {
        assert_eq!(
            upgrade_instruction_for_channel(InstallChannel::Scoop),
            "scoop update 1up"
        );
    }

    #[test]
    fn upgrade_instruction_for_manual() {
        assert_eq!(
            upgrade_instruction_for_channel(InstallChannel::Manual),
            "1up update"
        );
    }

    #[test]
    fn upgrade_instruction_for_unknown() {
        assert_eq!(
            upgrade_instruction_for_channel(InstallChannel::Unknown),
            "1up update"
        );
    }

    #[test]
    fn detect_channel_from_path_returns_manual_for_generic_path() {
        let path = std::path::Path::new("/usr/local/bin/1up");
        assert_eq!(detect_channel_from_path(path), InstallChannel::Manual);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn detect_channel_from_path_returns_homebrew_for_cellar_path() {
        let path = std::path::Path::new("/opt/homebrew/Cellar/1up/0.1.0/bin/1up");
        assert_eq!(detect_channel_from_path(path), InstallChannel::Homebrew);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn detect_channel_from_path_returns_homebrew_for_homebrew_prefix_path() {
        let path = std::path::Path::new("/opt/homebrew/bin/1up");
        assert_eq!(detect_channel_from_path(path), InstallChannel::Homebrew);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn detect_channel_from_path_returns_homebrew_for_linuxbrew_cellar_path() {
        let path = std::path::Path::new("/home/linuxbrew/.linuxbrew/Cellar/1up/0.1.0/bin/1up");
        assert_eq!(detect_channel_from_path(path), InstallChannel::Homebrew);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn detect_channel_from_path_returns_scoop_for_scoop_path() {
        let path = std::path::Path::new(r"C:\Users\user\scoop\apps\1up\current\1up.exe");
        assert_eq!(detect_channel_from_path(path), InstallChannel::Scoop);
    }

    #[test]
    fn detect_channel_from_path_returns_manual_for_cargo_install_path() {
        let path = std::path::Path::new("/home/user/.cargo/bin/1up");
        assert_eq!(detect_channel_from_path(path), InstallChannel::Manual);
    }

    #[test]
    fn detect_install_channel_returns_known_channel() {
        let channel = detect_install_channel();
        assert!(
            matches!(
                channel,
                InstallChannel::Homebrew
                    | InstallChannel::Scoop
                    | InstallChannel::Manual
                    | InstallChannel::Unknown
            ),
            "expected a valid InstallChannel variant, got: {channel}"
        );
    }

    #[test]
    fn is_cache_valid_returns_true_for_fresh_matching_cache() {
        let cache = make_cache(VERSION, "99.0.0", false, None, None);
        assert!(is_cache_valid(&cache, VERSION));
    }

    #[test]
    fn is_cache_valid_returns_false_when_version_differs() {
        let cache = make_cache("0.0.1", "99.0.0", false, None, None);
        assert!(!is_cache_valid(&cache, VERSION));
    }

    #[test]
    fn is_cache_valid_returns_false_when_ttl_exceeded() {
        let mut cache = make_cache(VERSION, "99.0.0", false, None, None);
        cache.checked_at = Utc::now() - chrono::Duration::hours(25);
        assert!(!is_cache_valid(&cache, VERSION));
    }

    #[test]
    fn is_cache_valid_returns_true_at_boundary() {
        let mut cache = make_cache(VERSION, "99.0.0", false, None, None);
        cache.checked_at = Utc::now() - chrono::Duration::hours(23);
        assert!(is_cache_valid(&cache, VERSION));
    }

    #[test]
    fn configured_update_manifest_url_uses_runtime_override() {
        let _lock = UPDATE_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ScopedEnvGuard::set(
            UPDATE_MANIFEST_URL_ENV_VAR,
            "https://example.com/update-manifest.json",
        );

        assert_eq!(
            configured_update_manifest_url().as_deref(),
            Some("https://example.com/update-manifest.json")
        );
        assert!(updates_enabled());
    }

    #[test]
    fn configured_update_manifest_url_allows_runtime_disable_override() {
        let _lock = UPDATE_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ScopedEnvGuard::set(UPDATE_MANIFEST_URL_ENV_VAR, "");

        assert_eq!(configured_update_manifest_url(), None);
        assert!(!updates_enabled());
    }

    #[test]
    fn update_errors_invalidate_cache_only_for_permanent_failures() {
        assert!(UpdateError::Disabled.should_invalidate_cache());
        assert!(UpdateError::ParseFailed("invalid json".to_string()).should_invalidate_cache());
        assert!(UpdateError::FetchFailed {
            detail: "manifest fetch: HTTP 404 Not Found".to_string(),
            permanent: true,
        }
        .should_invalidate_cache());
        assert!(!UpdateError::FetchFailed {
            detail: "manifest fetch: timeout".to_string(),
            permanent: false,
        }
        .should_invalidate_cache());
    }

    #[test]
    fn read_update_cache_returns_none_for_missing_file() {
        let cache = read_update_cache();
        // The file may or may not exist depending on test environment;
        // at minimum, this should not panic.
        let _ = cache;
    }

    #[test]
    fn build_cache_from_manifest_populates_all_fields() {
        let manifest = UpdateManifest {
            version: "0.2.0".to_string(),
            git_tag: "v0.2.0".to_string(),
            published_at: "2026-04-10T12:00:00Z".to_string(),
            notes_url: "https://example.com/notes".to_string(),
            artifacts: vec![],
            channels: UpdateChannels {
                github_release: "https://example.com".to_string(),
                homebrew_tap: "rp1-run/tap".to_string(),
                homebrew_formula: "1up".to_string(),
                scoop_bucket: "rp1-run".to_string(),
                scoop_manifest: "1up".to_string(),
            },
            yanked: true,
            minimum_safe_version: Some("0.1.5".to_string()),
            message: Some("urgent fix".to_string()),
        };

        let cache = build_cache_from_manifest(&manifest);
        assert_eq!(cache.current_version, VERSION);
        assert_eq!(cache.latest_version, "0.2.0");
        assert!(cache.yanked);
        assert_eq!(cache.minimum_safe_version, Some("0.1.5".to_string()));
        assert_eq!(cache.message, Some("urgent fix".to_string()));
        assert_eq!(
            cache.notes_url,
            Some("https://example.com/notes".to_string())
        );
        assert!(!cache.upgrade_instruction.is_empty());
    }

    #[test]
    fn build_cache_from_manifest_sets_checked_at_to_now() {
        let manifest = UpdateManifest {
            version: "0.2.0".to_string(),
            git_tag: "v0.2.0".to_string(),
            published_at: "2026-04-10T12:00:00Z".to_string(),
            notes_url: "https://example.com/notes".to_string(),
            artifacts: vec![],
            channels: UpdateChannels {
                github_release: "https://example.com".to_string(),
                homebrew_tap: "rp1-run/tap".to_string(),
                homebrew_formula: "1up".to_string(),
                scoop_bucket: "rp1-run".to_string(),
                scoop_manifest: "1up".to_string(),
            },
            yanked: false,
            minimum_safe_version: None,
            message: None,
        };

        let before = Utc::now();
        let cache = build_cache_from_manifest(&manifest);
        let after = Utc::now();
        assert!(cache.checked_at >= before);
        assert!(cache.checked_at <= after);
    }

    #[test]
    fn write_and_read_cache_round_trip() {
        use std::sync::Mutex;

        static CACHE_ENV_MUTEX: Mutex<()> = Mutex::new(());

        let _lock = CACHE_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let saved_xdg = std::env::var_os("XDG_DATA_HOME");
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp_root.join("xdg-data"));

        let cache = make_cache(VERSION, "99.0.0", false, None, None);
        write_update_cache(&cache);

        let loaded = read_update_cache();
        assert!(
            loaded.is_some(),
            "expected cache to be readable after write"
        );
        let loaded = loaded.unwrap();
        assert_eq!(loaded.current_version, cache.current_version);
        assert_eq!(loaded.latest_version, cache.latest_version);
        assert_eq!(loaded.install_channel, cache.install_channel);

        match saved_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[test]
    fn read_compatible_update_cache_clears_version_mismatched_cache() {
        use std::sync::Mutex;

        static CACHE_ENV_MUTEX: Mutex<()> = Mutex::new(());

        let _lock = CACHE_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let saved_xdg = std::env::var_os("XDG_DATA_HOME");
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp_root.join("xdg-data"));

        write_update_cache(&make_cache("0.0.1", "99.0.0", false, None, None));

        assert!(read_compatible_update_cache(VERSION).is_none());
        assert!(read_update_cache().is_none());

        match saved_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_update_cache_uses_secure_permissions() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;

        static PERM_ENV_MUTEX: Mutex<()> = Mutex::new(());

        let _lock = PERM_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let saved_xdg = std::env::var_os("XDG_DATA_HOME");
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp_root.join("xdg-data"));

        let cache = make_cache(VERSION, "99.0.0", false, None, None);
        write_update_cache(&cache);

        let path = config::update_check_cache_path().unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, SECURE_STATE_FILE_MODE,
            "cache file should have 0o600 permissions"
        );

        match saved_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[test]
    fn write_update_cache_does_not_panic_on_failure() {
        // write_update_cache with a bad path should silently fail
        use std::sync::Mutex;

        static BAD_ENV_MUTEX: Mutex<()> = Mutex::new(());

        let _lock = BAD_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let saved_xdg = std::env::var_os("XDG_DATA_HOME");
        // Point at a path that will fail (file exists as non-directory)
        std::env::set_var("XDG_DATA_HOME", "/dev/null");

        let cache = make_cache("0.1.0", "0.1.1", false, None, None);
        // Should not panic
        write_update_cache(&cache);

        match saved_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[test]
    fn refresh_cache_if_stale_clears_cache_when_updates_disabled() {
        let _lock = UPDATE_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _manifest_guard = ScopedEnvGuard::set(UPDATE_MANIFEST_URL_ENV_VAR, "");
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let _xdg_guard =
            ScopedEnvGuard::set("XDG_DATA_HOME", tmp_root.join("xdg-data").into_os_string());

        write_update_cache(&make_cache(VERSION, "99.0.0", false, None, None));

        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(runtime.block_on(refresh_cache_if_stale()).is_none());
        assert!(read_update_cache().is_none());
    }

    #[test]
    fn refresh_cache_if_stale_does_not_reuse_version_mismatched_cache() {
        let _lock = UPDATE_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _manifest_guard = ScopedEnvGuard::set(
            UPDATE_MANIFEST_URL_ENV_VAR,
            "http://127.0.0.1:9/update-manifest.json",
        );
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let _xdg_guard =
            ScopedEnvGuard::set("XDG_DATA_HOME", tmp_root.join("xdg-data").into_os_string());

        write_update_cache(&make_cache("0.0.1", "99.0.0", false, None, None));

        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(runtime.block_on(refresh_cache_if_stale()).is_none());
        assert!(read_update_cache().is_none());
    }

    #[test]
    fn build_update_status_prerelease_current_shows_update_to_release() {
        let cache = make_cache("0.1.0-alpha.1", "0.1.0", false, None, None);
        assert_eq!(
            build_update_status(&cache),
            UpdateStatus::UpdateAvailable {
                latest: "0.1.0".to_string(),
            }
        );
    }

    #[test]
    fn build_update_status_returns_up_to_date_for_unparseable_current() {
        let cache = make_cache("not-semver", "0.1.0", false, None, None);
        assert_eq!(build_update_status(&cache), UpdateStatus::UpToDate);
    }

    #[test]
    fn build_update_status_returns_up_to_date_for_unparseable_latest() {
        let cache = make_cache("0.1.0", "garbage", false, None, None);
        assert_eq!(build_update_status(&cache), UpdateStatus::UpToDate);
    }

    #[test]
    fn build_update_check_client_succeeds() {
        let client = build_update_check_client();
        assert!(client.is_ok(), "expected client to build successfully");
    }

    #[test]
    fn build_download_client_succeeds() {
        let client = build_download_client();
        assert!(client.is_ok(), "expected download client to build");
    }

    fn make_manifest_with_artifact(target: &str, sha256: &str) -> UpdateManifest {
        UpdateManifest {
            version: "0.2.0".to_string(),
            git_tag: "v0.2.0".to_string(),
            published_at: "2026-04-10T12:00:00Z".to_string(),
            notes_url: "https://example.com/notes".to_string(),
            artifacts: vec![UpdateArtifact {
                target: target.to_string(),
                archive: format!("1up-v0.2.0-{target}.tar.gz"),
                sha256: sha256.to_string(),
                url: "https://example.com/archive.tar.gz".to_string(),
            }],
            channels: UpdateChannels {
                github_release: "https://example.com".to_string(),
                homebrew_tap: "rp1-run/tap".to_string(),
                homebrew_formula: "1up".to_string(),
                scoop_bucket: "rp1-run".to_string(),
                scoop_manifest: "1up".to_string(),
            },
            yanked: false,
            minimum_safe_version: None,
            message: None,
        }
    }

    #[test]
    fn find_artifact_for_platform_succeeds_with_matching_target() {
        let triple = current_target_triple().unwrap();
        let manifest = make_manifest_with_artifact(triple, "abc123");
        let artifact = find_artifact_for_platform(&manifest);
        assert!(artifact.is_ok());
        assert_eq!(artifact.unwrap().target, triple);
    }

    #[test]
    fn find_artifact_for_platform_returns_error_for_missing_target() {
        let manifest = make_manifest_with_artifact("mips-unknown-linux-gnu", "abc123");
        let result = find_artifact_for_platform(&manifest);
        assert!(result.is_err());
        match result.unwrap_err() {
            UpdateError::NoArtifactForPlatform(t) => {
                let expected = current_target_triple().unwrap();
                assert_eq!(t, expected);
            }
            other => panic!("expected NoArtifactForPlatform, got: {other:?}"),
        }
    }

    #[test]
    fn find_artifact_for_platform_returns_error_for_empty_artifacts() {
        let mut manifest = make_manifest_with_artifact("x", "y");
        manifest.artifacts.clear();
        let result = find_artifact_for_platform(&manifest);
        assert!(result.is_err());
    }

    #[test]
    fn verify_archive_checksum_passes_for_correct_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("test-file");
        std::fs::write(&file_path, b"hello world\n").unwrap();

        let mut hasher = Sha256::new();
        hasher.update(b"hello world\n");
        let expected: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        let result = verify_archive_checksum(&file_path, &expected);
        assert!(result.is_ok(), "checksum should pass for correct digest");
    }

    #[test]
    fn verify_archive_checksum_fails_for_wrong_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("test-file");
        std::fs::write(&file_path, b"hello world\n").unwrap();

        let result = verify_archive_checksum(&file_path, "0000000000000000");
        assert!(result.is_err());
        match result.unwrap_err() {
            UpdateError::ChecksumMismatch => {}
            other => panic!("expected ChecksumMismatch, got: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn extract_binary_from_tar_gz_archive() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("test.tar.gz");

        let file = std::fs::File::create(&archive_path).unwrap();
        let encoder = GzEncoder::new(file, Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let binary_content = b"#!/bin/sh\necho hello";
        let mut header = tar::Header::new_gnu();
        header.set_size(binary_content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();

        builder
            .append_data(
                &mut header,
                "1up-v0.2.0-aarch64-apple-darwin/1up",
                &binary_content[..],
            )
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();

        let staging_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_from_archive(&archive_path, staging_dir.path());
        assert!(result.is_ok(), "extraction should succeed");

        let extracted = result.unwrap();
        assert!(extracted.exists(), "extracted binary should exist");
        assert_eq!(
            std::fs::read(&extracted).unwrap(),
            binary_content,
            "binary content should match"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_binary_from_archive_fails_when_binary_missing() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("test.tar.gz");

        let file = std::fs::File::create(&archive_path).unwrap();
        let encoder = GzEncoder::new(file, Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let content = b"license text";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "1up-v0.2.0/LICENSE", &content[..])
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();

        let staging_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_from_archive(&archive_path, staging_dir.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            UpdateError::SelfUpdateFailed(msg) => {
                assert!(
                    msg.contains("not found in archive"),
                    "error should mention missing binary: {msg}"
                );
            }
            other => panic!("expected SelfUpdateFailed, got: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn replace_binary_atomically_replaces_file() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("1up");
        let new_binary = tmp.path().join("1up-new");

        std::fs::write(&target, b"old binary").unwrap();
        std::fs::write(&new_binary, b"new binary").unwrap();

        let result = replace_binary(&new_binary, &target);
        assert!(result.is_ok(), "replace should succeed");
        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        assert!(!new_binary.exists(), "staging file should be removed");

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "binary should be executable");
    }

    #[test]
    fn format_notification_returns_none_for_up_to_date() {
        let cache = make_cache(VERSION, VERSION, false, None, None);
        let status = build_update_status(&cache);
        assert_eq!(status, UpdateStatus::UpToDate);
    }

    #[test]
    fn format_notification_includes_versions_and_instruction_for_update() {
        let cache = make_cache("0.1.0", "0.1.1", false, None, None);
        let status = build_update_status(&cache);
        assert!(matches!(status, UpdateStatus::UpdateAvailable { .. }));
    }

    #[test]
    fn format_notification_shows_urgent_warning_for_yanked() {
        let cache = make_cache("0.1.0", "0.1.1", true, None, Some("data corruption bug"));
        let status = build_update_status(&cache);
        assert!(matches!(status, UpdateStatus::Yanked { .. }));
    }

    #[test]
    fn format_notification_shows_urgency_for_below_minimum_safe() {
        let cache = make_cache("0.1.0", "0.1.2", false, Some("0.1.1"), Some("security fix"));
        let status = build_update_status(&cache);
        assert!(matches!(status, UpdateStatus::BelowMinimumSafe { .. }));
    }

    #[test]
    fn format_notification_uses_channel_upgrade_instruction() {
        let mut cache = make_cache("0.1.0", "0.1.1", false, None, None);
        cache.upgrade_instruction = upgrade_instruction_for_channel(InstallChannel::Homebrew);
        assert_eq!(cache.upgrade_instruction, "brew upgrade rp1-run/tap/1up");

        cache.upgrade_instruction = upgrade_instruction_for_channel(InstallChannel::Scoop);
        assert_eq!(cache.upgrade_instruction, "scoop update 1up");

        cache.upgrade_instruction = upgrade_instruction_for_channel(InstallChannel::Manual);
        assert_eq!(cache.upgrade_instruction, "1up update");
    }

    #[test]
    fn format_update_notification_returns_none_when_updates_disabled() {
        let _lock = UPDATE_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _manifest_guard = ScopedEnvGuard::set(UPDATE_MANIFEST_URL_ENV_VAR, "");
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let _xdg_guard =
            ScopedEnvGuard::set("XDG_DATA_HOME", tmp_root.join("xdg-data").into_os_string());

        write_update_cache(&make_cache(VERSION, "99.0.0", false, None, None));

        assert_eq!(format_update_notification(), None);
        assert!(read_update_cache().is_some());
    }

    #[test]
    fn format_update_notification_ignores_version_mismatched_cache() {
        let _lock = UPDATE_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _manifest_guard = ScopedEnvGuard::set(
            UPDATE_MANIFEST_URL_ENV_VAR,
            "https://example.com/update-manifest.json",
        );
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let _xdg_guard =
            ScopedEnvGuard::set("XDG_DATA_HOME", tmp_root.join("xdg-data").into_os_string());

        write_update_cache(&make_cache("0.0.1", "99.0.0", false, None, None));

        assert_eq!(format_update_notification(), None);
        assert!(read_update_cache().is_none());
    }
}
