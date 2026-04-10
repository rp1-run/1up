use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Machine-readable update manifest published at a stable URL.
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

/// Returns `true` when `latest_version` is strictly newer than `current_version`
/// according to semver ordering.
pub fn is_newer_version(current_version: &str, latest_version: &str) -> bool {
    let current = match semver::Version::parse(current_version) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let latest = match semver::Version::parse(latest_version) {
        Ok(v) => v,
        Err(_) => return false,
    };
    latest > current
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

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
    fn is_newer_version_compares_correctly() {
        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.0", "1.0.0"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.2.0", "0.1.0"));
    }

    #[test]
    fn is_newer_version_prerelease_sorts_before_release() {
        assert!(is_newer_version("0.1.0-alpha.1", "0.1.0"));
        assert!(!is_newer_version("0.1.0", "0.1.0-alpha.1"));
    }

    #[test]
    fn is_newer_version_handles_unparseable_versions() {
        assert!(!is_newer_version("not-a-version", "0.1.0"));
        assert!(!is_newer_version("0.1.0", "not-a-version"));
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
}
