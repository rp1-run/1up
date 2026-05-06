use crate::shared::constants::DEFAULT_INDEX_CONTEXT_ID;
use crate::shared::types::{BranchStatus, WorktreeContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchScope {
    context_id: String,
    branch_status: BranchStatus,
}

impl SearchScope {
    pub fn new(context_id: impl Into<String>, branch_status: BranchStatus) -> Self {
        Self {
            context_id: context_id.into(),
            branch_status,
        }
    }

    pub fn default_context() -> Self {
        Self::new(DEFAULT_INDEX_CONTEXT_ID, BranchStatus::Unknown)
    }

    pub fn from_worktree_context(context: &WorktreeContext) -> Self {
        Self {
            context_id: context.context_id.clone(),
            branch_status: context.branch_status,
        }
    }

    pub fn context_id(&self) -> &str {
        &self.context_id
    }

    pub fn degraded_reason(&self) -> Option<String> {
        match self.branch_status {
            BranchStatus::Unreadable | BranchStatus::Unknown => Some(format!(
                "branch context is {}; results are scoped to the active worktree context but cannot be presented as definitively branch-filtered",
                self.branch_status.as_str()
            )),
            BranchStatus::Named | BranchStatus::Detached => None,
        }
    }
}
