use crate::shared::constants::DEFAULT_INDEX_CONTEXT_ID;
use crate::shared::types::{BranchStatus, WorktreeContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchScope {
    context_id: String,
    branch_status: BranchStatus,
    path_prefix: Option<String>,
}

impl SearchScope {
    pub fn new(context_id: impl Into<String>, branch_status: BranchStatus) -> Self {
        Self {
            context_id: context_id.into(),
            branch_status,
            path_prefix: None,
        }
    }

    pub fn default_context() -> Self {
        Self::new(DEFAULT_INDEX_CONTEXT_ID, BranchStatus::Unknown)
    }

    pub fn from_worktree_context(context: &WorktreeContext) -> Self {
        Self {
            context_id: context.context_id.clone(),
            branch_status: context.branch_status,
            path_prefix: None,
        }
    }

    pub fn context_id(&self) -> &str {
        &self.context_id
    }

    /// Scopes retrieval to a repo-relative directory prefix (wired from the
    /// request layer in T6). Leading/trailing slashes are trimmed; a prefix
    /// that is empty after trimming clears scoping (full-repo, unchanged).
    #[allow(dead_code)]
    pub fn with_path_prefix(mut self, prefix: impl Into<String>) -> Self {
        let trimmed = prefix.into();
        let trimmed = trimmed.trim_matches('/');
        self.path_prefix = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        self
    }

    pub fn path_prefix(&self) -> Option<&str> {
        self.path_prefix.as_deref()
    }

    /// Builds the bound `LIKE` pattern for the active `path_prefix`, or `None`
    /// when the scope is unbounded (full-repo). Callers apply it against
    /// `(file_path || '/') LIKE ?N ESCAPE '\'`: appending `'/'` to the stored
    /// file path before matching means the prefix itself and any of its
    /// descendants match, while a sibling that merely shares the prefix as a
    /// string prefix does not (`src/foo` matches `src/foo` and
    /// `src/foo/bar.rs`, but not `src/foobar.rs`). SQLite `LIKE` metacharacters
    /// (`\`, `%`, `_`) occurring in the prefix are escaped so literal path
    /// segments containing them are never treated as wildcards.
    pub fn path_prefix_like_pattern(&self) -> Option<String> {
        self.path_prefix.as_deref().map(escape_like_prefix)
    }

    pub fn degraded_reason(&self) -> Option<String> {
        match self.branch_status {
            BranchStatus::Unreadable | BranchStatus::Unknown => {
                Some(self.branch_status.branch_scope_caveat())
            }
            BranchStatus::Named | BranchStatus::Detached => None,
        }
    }
}

fn escape_like_prefix(prefix: &str) -> String {
    let mut escaped = String::with_capacity(prefix.len() + 2);
    for ch in prefix.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push_str("/%");
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_prefix_defaults_to_unscoped() {
        let scope = SearchScope::default_context();
        assert_eq!(scope.path_prefix(), None);
        assert_eq!(scope.path_prefix_like_pattern(), None);
    }

    #[test]
    fn path_prefix_trims_slashes() {
        let scope = SearchScope::default_context().with_path_prefix("/src/foo/");
        assert_eq!(scope.path_prefix(), Some("src/foo"));
    }

    #[test]
    fn empty_path_prefix_clears_scoping() {
        let scope = SearchScope::default_context()
            .with_path_prefix("src/foo")
            .with_path_prefix("");
        assert_eq!(scope.path_prefix(), None);
    }

    #[test]
    fn like_pattern_appends_directory_boundary_wildcard() {
        let scope = SearchScope::default_context().with_path_prefix("src/foo");
        assert_eq!(scope.path_prefix_like_pattern().unwrap(), "src/foo/%");
    }

    #[test]
    fn like_pattern_escapes_sqlite_wildcards_in_prefix() {
        let scope = SearchScope::default_context().with_path_prefix("src/f%o_o");
        let pattern = scope.path_prefix_like_pattern().unwrap();
        assert_eq!(pattern, "src/f\\%o\\_o/%");
    }
}
