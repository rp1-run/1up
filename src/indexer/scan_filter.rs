use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::shared::constants::DEFAULT_SECRET_GLOBS;
use crate::shared::errors::{IndexingError, OneupError};

/// Shared inclusion/exclusion predicate reused by the indexer scanner,
/// `oneup_context`, and the daemon watcher so exclusion rules cannot drift
/// between the three consumers.
///
/// Precedence (highest to lowest): secret pattern (non-overridable) >
/// scope_globs (exclusive cone, only when scoped — the scoped-indexing cost
/// boundary, which configured includes must not punch through) > configured
/// include glob or dotfile-directory override > configured user exclude glob >
/// default dotfile/dot-directory hiding > include by default.
///
/// Pure and I/O-free: callers supply the repo-relative path and whether it
/// names a directory.
pub struct ScanFilter {
    secret_globs: GlobSet,
    include_globs: GlobSet,
    exclude_globs: GlobSet,
    override_dirs: Vec<PathBuf>,
    /// Exclusive scope patterns (e.g., "services/**") populated only when scope
    /// filtering is active. When set, only files matching scope_globs are included.
    /// This is distinct from include_globs which only guarantees inclusion.
    scope_globs: GlobSet,
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
            scope_globs: build_globset(Vec::<String>::new())?,
        })
    }

    /// Build a filter with exclusive scope patterns (for scoped indexing).
    /// Scope globs define an exclusive cone: only files matching any scope glob
    /// are included. This is used when scope_roots are applied.
    pub fn with_scope_globs(
        include_globs: &[String],
        exclude_globs: &[String],
        override_dirs: &[String],
        scope_globs_patterns: &[String],
    ) -> Result<Self, OneupError> {
        Ok(Self {
            secret_globs: build_globset(DEFAULT_SECRET_GLOBS)?,
            include_globs: build_globset(include_globs)?,
            exclude_globs: build_globset(exclude_globs)?,
            override_dirs: override_dirs.iter().map(PathBuf::from).collect(),
            scope_globs: build_globset(scope_globs_patterns)?,
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
        // Scope filtering runs BEFORE include/override: the scope cone is the
        // feature's cost boundary, so a configured include
        // glob or override dir must not pull out-of-cone files into a scoped
        // index. Directories always descend so in-cone files stay reachable.
        if !self.scope_globs.is_empty() && !is_dir && !glob_matches(&self.scope_globs, rel_path) {
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

    fn scoped_filter(include: &[&str], overrides: &[&str], scope: &[&str]) -> ScanFilter {
        let include: Vec<String> = include.iter().map(|s| s.to_string()).collect();
        let overrides: Vec<String> = overrides.iter().map(|s| s.to_string()).collect();
        let scope: Vec<String> = scope.iter().map(|s| s.to_string()).collect();
        ScanFilter::with_scope_globs(&include, &[], &overrides, &scope).unwrap()
    }

    #[test]
    fn scope_cone_excludes_out_of_scope_file_despite_include_glob() {
        // The scope cone is the cost boundary; a configured
        // include glob must not pull out-of-cone files into a scoped index.
        let f = scoped_filter(&["**/*.ts"], &[], &["services/**"]);
        assert!(f.is_excluded(Path::new("web/app.ts"), false));
        assert!(!f.is_excluded(Path::new("services/auth/api.ts"), false));
    }

    #[test]
    fn scope_cone_excludes_out_of_scope_file_despite_override_dir() {
        let f = scoped_filter(&[], &[".github"], &["services/**"]);
        assert!(f.is_excluded(Path::new(".github/workflows/ci.yml"), false));
        // Directories still descend under scope so in-cone files stay reachable.
        assert!(!f.is_excluded(Path::new(".github"), true));
    }

    #[test]
    fn include_glob_still_wins_over_exclude_within_scope() {
        let f = ScanFilter::with_scope_globs(
            &["services/auth/keep.gen.ts".to_string()],
            &["**/*.gen.ts".to_string()],
            &[],
            &["services/**".to_string()],
        )
        .unwrap();
        assert!(!f.is_excluded(Path::new("services/auth/keep.gen.ts"), false));
        assert!(f.is_excluded(Path::new("services/auth/other.gen.ts"), false));
    }

    #[test]
    fn secret_pattern_excluded_regardless_of_include_glob() {
        // Secret exclusion glob expansion. Verify all patterns from
        // DEFAULT_SECRET_GLOBS are excluded even when include glob is "*".
        let f = filter(&["*"], &[], &[]);
        // Original 4 patterns
        assert!(f.is_excluded(Path::new("secrets/credentials.json"), false));
        assert!(f.is_excluded(Path::new("id_rsa.pem"), false));
        assert!(f.is_excluded(Path::new("service.key"), false));
        assert!(f.is_excluded(Path::new(".env"), false));
        assert!(f.is_excluded(Path::new("config/.env"), false));
        // Expanded patterns
        assert!(f.is_excluded(Path::new("gcp-service-account.json"), false));
        assert!(f.is_excluded(Path::new("service-account-key.json"), false));
        assert!(f.is_excluded(Path::new("secrets.yaml"), false));
        assert!(f.is_excluded(Path::new("secrets.yml"), false));
        assert!(f.is_excluded(Path::new(".netrc"), false));
        assert!(f.is_excluded(Path::new(".pgpass"), false));
        assert!(f.is_excluded(Path::new(".git-credentials"), false));
        assert!(f.is_excluded(Path::new(".aws/credentials"), false));
        assert!(f.is_excluded(Path::new("id_rsa"), false));
        assert!(f.is_excluded(Path::new("id_rsa.pub"), false));
        assert!(f.is_excluded(Path::new("id_ed25519"), false));
        assert!(f.is_excluded(Path::new("id_ed25519.pub"), false));
        assert!(f.is_excluded(Path::new("cert.p12"), false));
        assert!(f.is_excluded(Path::new("cert.pfx"), false));
        assert!(f.is_excluded(Path::new(".env.local"), false));
        assert!(f.is_excluded(Path::new(".env.prod"), false));
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
