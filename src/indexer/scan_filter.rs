use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::shared::errors::{IndexingError, OneupError};

/// Default-on secret-file patterns, excluded regardless of configuration.
const DEFAULT_SECRET_GLOBS: &[&str] = &["*.pem", "*.key", "credentials.json", ".env"];

/// Shared inclusion/exclusion predicate reused by the indexer scanner,
/// `oneup_context`, and the daemon watcher so exclusion rules cannot drift
/// between the three consumers.
///
/// Precedence (highest to lowest): secret pattern (non-overridable) >
/// configured include glob or dotfile-directory override > configured user
/// exclude glob > default dotfile/dot-directory hiding > include by default.
///
/// Pure and I/O-free: callers supply the repo-relative path and whether it
/// names a directory.
pub struct ScanFilter {
    secret_globs: GlobSet,
    include_globs: GlobSet,
    exclude_globs: GlobSet,
    override_dirs: Vec<PathBuf>,
}

fn build_globset<S: AsRef<str>>(
    patterns: impl IntoIterator<Item = S>,
) -> Result<GlobSet, OneupError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.as_ref();
        let glob = Glob::new(pattern)
            .map_err(|e| IndexingError::Scan(format!("invalid glob pattern '{pattern}': {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| IndexingError::Scan(format!("failed to build glob set: {e}")))
        .map_err(OneupError::from)
}

fn glob_matches(globset: &GlobSet, rel_path: &Path) -> bool {
    if globset.is_match(rel_path) {
        return true;
    }
    rel_path
        .file_name()
        .is_some_and(|name| globset.is_match(name))
}

fn is_dotfile(rel_path: &Path) -> bool {
    rel_path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|s| s.starts_with('.') && s != "." && s != "..")
    })
}

impl ScanFilter {
    /// Build a filter from per-project include/exclude glob patterns and
    /// dotfile-directory override paths (repo-relative, e.g. `.github/workflows`).
    pub fn new(
        include_globs: &[String],
        exclude_globs: &[String],
        override_dirs: &[String],
    ) -> Result<Self, OneupError> {
        Ok(Self {
            secret_globs: build_globset(DEFAULT_SECRET_GLOBS)?,
            include_globs: build_globset(include_globs)?,
            exclude_globs: build_globset(exclude_globs)?,
            override_dirs: override_dirs.iter().map(PathBuf::from).collect(),
        })
    }

    fn matches_override(&self, rel_path: &Path, is_dir: bool) -> bool {
        self.override_dirs
            .iter()
            .any(|dir| rel_path.starts_with(dir) || (is_dir && dir.starts_with(rel_path)))
    }

    /// Returns `true` when `rel_path` should be excluded, applying the fixed
    /// precedence documented on the type.
    pub fn is_excluded(&self, rel_path: &Path, is_dir: bool) -> bool {
        if glob_matches(&self.secret_globs, rel_path) {
            return true;
        }
        if glob_matches(&self.include_globs, rel_path) || self.matches_override(rel_path, is_dir) {
            return false;
        }
        if glob_matches(&self.exclude_globs, rel_path) {
            return true;
        }
        if is_dotfile(rel_path) {
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(include: &[&str], exclude: &[&str], overrides: &[&str]) -> ScanFilter {
        let include: Vec<String> = include.iter().map(|s| s.to_string()).collect();
        let exclude: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
        let overrides: Vec<String> = overrides.iter().map(|s| s.to_string()).collect();
        ScanFilter::new(&include, &exclude, &overrides).unwrap()
    }

    #[test]
    fn secret_pattern_excluded_regardless_of_include_glob() {
        let f = filter(&["*"], &[], &[]);
        assert!(f.is_excluded(Path::new("secrets/credentials.json"), false));
        assert!(f.is_excluded(Path::new("id_rsa.pem"), false));
        assert!(f.is_excluded(Path::new("service.key"), false));
        assert!(f.is_excluded(Path::new(".env"), false));
        assert!(f.is_excluded(Path::new("config/.env"), false));
    }

    #[test]
    fn include_glob_beats_user_exclude_glob() {
        let f = filter(&["*.rs"], &["*.rs"], &[]);
        assert!(!f.is_excluded(Path::new("src/main.rs"), false));
    }

    #[test]
    fn dotfile_override_re_admits_configured_directory_and_ancestors() {
        let f = filter(&[], &[], &[".github/workflows"]);
        assert!(!f.is_excluded(Path::new(".github"), true));
        assert!(!f.is_excluded(Path::new(".github/workflows"), true));
        assert!(!f.is_excluded(Path::new(".github/workflows/ci.yml"), false));
        assert!(f.is_excluded(Path::new(".github/ISSUE_TEMPLATE"), true));
        assert!(f.is_excluded(Path::new(".github/foo.txt"), false));
        assert!(f.is_excluded(Path::new(".hidden.rs"), false));
        assert!(f.is_excluded(Path::new(".secret"), true));
    }

    #[test]
    fn default_includes_normal_non_dotfile_paths() {
        let f = filter(&[], &[], &[]);
        assert!(!f.is_excluded(Path::new("src/main.rs"), false));
        assert!(!f.is_excluded(Path::new("random.txt"), false));
    }

    #[test]
    fn user_exclude_glob_excludes_when_not_included() {
        let f = filter(&[], &["*.log"], &[]);
        assert!(f.is_excluded(Path::new("debug.log"), false));
        assert!(!f.is_excluded(Path::new("main.rs"), false));
    }

    #[test]
    fn dotfile_excluded_by_default_with_no_override() {
        let f = filter(&[], &[], &[]);
        assert!(f.is_excluded(Path::new(".hidden.rs"), false));
        assert!(f.is_excluded(Path::new(".secret"), true));
        assert!(f.is_excluded(Path::new(".secret/leak.rs"), false));
    }
}
