use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::shared::config;
use crate::shared::constants::{SECURE_STATE_FILE_MODE, XDG_STATE_DIR_MODE};
use crate::shared::errors::{DaemonError, OneupError};
use crate::shared::fs::{atomic_replace, ensure_secure_xdg_root, validate_regular_file_path};
use crate::shared::project::{canonical_project_root, resolve_project_root};
use crate::shared::types::{BranchStatus, IndexingConfig, WorktreeContext, WorktreeRole};

const REGISTRY_LOCK_FILE: &str = "projects.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub project_id: String,
    pub project_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_worktree_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_role: Option<WorktreeRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_status: Option<BranchStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub registered_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<IndexingConfig>,
}

impl ProjectEntry {
    pub fn source_root(&self) -> &Path {
        self.source_root.as_deref().unwrap_or(&self.project_root)
    }

    #[allow(dead_code)]
    pub fn context_id(&self) -> String {
        self.context_id
            .clone()
            .unwrap_or_else(|| legacy_context_id(&self.project_root, self.source_root()))
    }

    #[allow(dead_code)]
    pub fn main_worktree_root(&self) -> &Path {
        self.main_worktree_root
            .as_deref()
            .unwrap_or(&self.project_root)
    }

    #[allow(dead_code)]
    pub fn worktree_role(&self) -> WorktreeRole {
        self.worktree_role.unwrap_or(WorktreeRole::Main)
    }

    pub fn branch_status(&self) -> BranchStatus {
        self.branch_status.unwrap_or(BranchStatus::Unknown)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Registry {
    pub projects: Vec<ProjectEntry>,
}

impl Registry {
    pub fn load() -> Result<Self, OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        Self::load_from_path_with_repair(&config::projects_registry_path()?, &xdg_root)
    }

    #[allow(dead_code)]
    pub fn save(&self) -> Result<(), OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        self.save_to_path(&config::projects_registry_path()?, &xdg_root)
    }

    /// Read the registry from disk, normalizing and collapsing duplicate
    /// entries in memory. Never writes: callers already holding the registry
    /// lock (the `*_at_path` mutators) persist through their own `save_to_path`,
    /// and unlocked readers must not clobber concurrent registrations.
    fn load_from_path(path: &Path, approved_root: &Path) -> Result<Self, OneupError> {
        Ok(Self::read_deduplicated(path, approved_root)?.0)
    }

    /// Load the registry and, if the unlocked read found duplicate entries,
    /// repair the on-disk file under the registry lock. Repair is best-effort:
    /// a lock, re-read, or write failure is logged and swallowed so a read-only
    /// or racing environment still receives the clean in-memory registry. The
    /// on-disk duplicates self-heal on a later successful load or registration.
    fn load_from_path_with_repair(path: &Path, approved_root: &Path) -> Result<Self, OneupError> {
        let (registry, had_duplicates) = Self::read_deduplicated(path, approved_root)?;
        if !had_duplicates {
            return Ok(registry);
        }

        match Self::repair_duplicates_at_path(path, approved_root) {
            Ok(repaired) => Ok(repaired),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to persist deduplicated project registry; using in-memory dedup"
                );
                Ok(registry)
            }
        }
    }

    /// Persist a deduplicated registry under the registry lock. Re-reads the
    /// file after acquiring the lock so a registration saved between an
    /// unlocked read and this repair is preserved, and saves only when the
    /// fresh state actually contained duplicates. Callers must not already
    /// hold the registry lock: the flock is not re-entrant across descriptors.
    fn repair_duplicates_at_path(path: &Path, approved_root: &Path) -> Result<Self, OneupError> {
        let _lock = acquire_registry_lock(approved_root)?;
        let (fresh, had_duplicates) = Self::read_deduplicated(path, approved_root)?;
        if had_duplicates {
            fresh.save_to_path(path, approved_root)?;
        }
        Ok(fresh)
    }

    /// Read, parse, normalize, and collapse the registry purely in memory.
    /// Returns the deduplicated registry and whether any duplicates were
    /// removed.
    fn read_deduplicated(path: &Path, approved_root: &Path) -> Result<(Self, bool), OneupError> {
        let path = validate_regular_file_path(path, approved_root).map_err(|err| {
            DaemonError::WatcherError(format!("failed to validate registry path: {err}"))
        })?;
        if !path.exists() {
            return Ok((Self::default(), false));
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| DaemonError::WatcherError(format!("failed to read registry: {e}")))?;

        let mut registry: Registry = serde_json::from_str(&content)
            .map_err(|e| DaemonError::WatcherError(format!("failed to parse registry: {e}")))?;
        registry.normalize_entries();
        let had_duplicates = registry.collapse_duplicate_entries();

        Ok((registry, had_duplicates))
    }

    fn save_to_path(&self, path: &Path, approved_root: &Path) -> Result<(), OneupError> {
        let content = serde_json::to_vec_pretty(self)
            .map_err(|e| DaemonError::WatcherError(format!("failed to serialize registry: {e}")))?;
        atomic_replace(
            path,
            &content,
            approved_root,
            XDG_STATE_DIR_MODE,
            SECURE_STATE_FILE_MODE,
        )
        .map_err(|e| DaemonError::WatcherError(format!("failed to write registry: {e}")))?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn register(
        &mut self,
        project_id: &str,
        project_root: &Path,
        indexing: Option<IndexingConfig>,
    ) -> Result<(), OneupError> {
        self.register_with_source(project_id, project_root, project_root, indexing)
    }

    pub fn register_with_source(
        &mut self,
        project_id: &str,
        project_root: &Path,
        source_root: &Path,
        indexing: Option<IndexingConfig>,
    ) -> Result<(), OneupError> {
        let context = registration_context(project_root, source_root);
        self.register_with_context(project_id, &context, indexing)
    }

    pub fn register_with_context(
        &mut self,
        project_id: &str,
        context: &WorktreeContext,
        indexing: Option<IndexingConfig>,
    ) -> Result<(), OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        self.register_context_at_path(
            project_id,
            context,
            indexing,
            &config::projects_registry_path()?,
            &xdg_root,
        )
    }

    #[allow(dead_code)]
    pub fn deregister(&mut self, project_root: &Path) -> Result<bool, OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        self.deregister_at_path(project_root, &config::projects_registry_path()?, &xdg_root)
    }

    pub fn deregister_context(&mut self, context: &WorktreeContext) -> Result<bool, OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        self.deregister_context_at_path(context, &config::projects_registry_path()?, &xdg_root)
    }

    /// Remove every registry entry whose context id is in `context_ids`. Used by
    /// `1up gc` to stop the daemon from re-watching (and so re-indexing) contexts
    /// whose rows were just pruned from the shared index. Returns how many entries
    /// were removed.
    pub fn deregister_context_ids(
        &mut self,
        context_ids: &HashSet<String>,
    ) -> Result<usize, OneupError> {
        let xdg_root = ensure_secure_xdg_root()?;
        self.deregister_context_ids_at_path(
            context_ids,
            &config::projects_registry_path()?,
            &xdg_root,
        )
    }

    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    pub fn contains_context(&self, context: &WorktreeContext) -> bool {
        let canonical = canonical_project_root(&context.state_root);
        let canonical_source = canonical_project_root(&context.source_root);
        self.projects
            .iter()
            .any(|entry| entry_matches_context(entry, &canonical, &canonical_source, context))
    }

    #[allow(dead_code)]
    pub fn indexing_config_for(&self, project_root: &Path) -> Option<&IndexingConfig> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());

        self.projects
            .iter()
            .find(|project| project.project_root == canonical)
            .and_then(|project| project.indexing.as_ref())
    }

    pub fn indexing_config_for_context(
        &self,
        context: &WorktreeContext,
    ) -> Option<&IndexingConfig> {
        let canonical = canonical_project_root(&context.state_root);
        let canonical_source = canonical_project_root(&context.source_root);
        self.projects
            .iter()
            .find(|entry| entry_matches_context(entry, &canonical, &canonical_source, context))
            .and_then(|project| project.indexing.as_ref())
    }

    #[allow(dead_code)]
    pub fn project_roots(&self) -> Vec<PathBuf> {
        self.projects
            .iter()
            .map(|p| p.project_root.clone())
            .collect()
    }

    #[allow(dead_code)]
    fn register_at_path(
        &mut self,
        project_id: &str,
        project_root: &Path,
        source_root: &Path,
        indexing: Option<IndexingConfig>,
        path: &Path,
        approved_root: &Path,
    ) -> Result<(), OneupError> {
        let context = registration_context(project_root, source_root);
        self.register_context_at_path(project_id, &context, indexing, path, approved_root)
    }

    fn register_context_at_path(
        &mut self,
        project_id: &str,
        context: &WorktreeContext,
        indexing: Option<IndexingConfig>,
        path: &Path,
        approved_root: &Path,
    ) -> Result<(), OneupError> {
        let _lock = acquire_registry_lock(approved_root)?;
        let mut latest = Self::load_from_path(path, approved_root)?;
        latest.upsert_project(project_id, context, indexing);
        latest.save_to_path(path, approved_root)?;
        *self = latest;
        Ok(())
    }

    #[allow(dead_code)]
    fn deregister_at_path(
        &mut self,
        project_root: &Path,
        path: &Path,
        approved_root: &Path,
    ) -> Result<bool, OneupError> {
        let _lock = acquire_registry_lock(approved_root)?;
        let mut latest = Self::load_from_path(path, approved_root)?;
        let removed = latest.remove_project(project_root);
        if removed {
            latest.save_to_path(path, approved_root)?;
        }
        *self = latest;
        Ok(removed)
    }

    fn deregister_context_at_path(
        &mut self,
        context: &WorktreeContext,
        path: &Path,
        approved_root: &Path,
    ) -> Result<bool, OneupError> {
        let _lock = acquire_registry_lock(approved_root)?;
        let mut latest = Self::load_from_path(path, approved_root)?;
        let removed = latest.remove_context(context);
        if removed {
            latest.save_to_path(path, approved_root)?;
        }
        *self = latest;
        Ok(removed)
    }

    fn deregister_context_ids_at_path(
        &mut self,
        context_ids: &HashSet<String>,
        path: &Path,
        approved_root: &Path,
    ) -> Result<usize, OneupError> {
        let _lock = acquire_registry_lock(approved_root)?;
        let mut latest = Self::load_from_path(path, approved_root)?;
        let before = latest.projects.len();
        latest
            .projects
            .retain(|entry| !context_ids.contains(&entry.context_id()));
        let removed = before - latest.projects.len();
        if removed > 0 {
            latest.save_to_path(path, approved_root)?;
        }
        *self = latest;
        Ok(removed)
    }

    fn upsert_project(
        &mut self,
        project_id: &str,
        context: &WorktreeContext,
        indexing: Option<IndexingConfig>,
    ) {
        self.normalize_entries();
        let canonical = canonical_project_root(&context.state_root);
        let canonical_source = canonical_project_root(&context.source_root);

        if let Some(existing) = self
            .projects
            .iter_mut()
            .find(|entry| entry_matches_context(entry, &canonical, &canonical_source, context))
        {
            existing.project_id = project_id.to_string();
            apply_context(existing, context, &canonical, &canonical_source);
            if let Some(indexing) = indexing {
                existing.indexing = Some(indexing);
            }
            return;
        }

        self.projects.push(ProjectEntry {
            project_id: project_id.to_string(),
            project_root: canonical.clone(),
            source_root: stored_source_root(&canonical, &canonical_source),
            context_id: Some(context.context_id.clone()),
            main_worktree_root: Some(canonical_project_root(&context.main_worktree_root)),
            worktree_role: Some(context.worktree_role),
            branch_name: context.branch_name.clone(),
            branch_ref: context.branch_ref.clone(),
            branch_status: Some(context.branch_status),
            head_oid: context.head_oid.clone(),
            registered_at: chrono::Utc::now().to_rfc3339(),
            indexing,
        });
    }

    #[allow(dead_code)]
    fn remove_project(&mut self, project_root: &Path) -> bool {
        let canonical = canonical_project_root(project_root);
        let before = self.projects.len();
        self.projects.retain(|p| p.project_root != canonical);
        self.projects.len() < before
    }

    fn remove_context(&mut self, context: &WorktreeContext) -> bool {
        let canonical = canonical_project_root(&context.state_root);
        let canonical_source = canonical_project_root(&context.source_root);
        let before = self.projects.len();
        self.projects
            .retain(|entry| !entry_matches_context(entry, &canonical, &canonical_source, context));
        if self.projects.len() < before {
            return true;
        }

        let mut matching_indexes = self
            .projects
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| (entry.project_root == canonical).then_some(index));
        let Some(index) = matching_indexes.next() else {
            return false;
        };
        if matching_indexes.next().is_some() {
            return false;
        }

        self.projects.remove(index);
        true
    }

    fn normalize_entries(&mut self) {
        for entry in &mut self.projects {
            let canonical = canonical_project_root(&entry.project_root);
            let canonical_source = canonical_project_root(entry.source_root());
            entry.project_root = canonical.clone();
            entry.source_root = stored_source_root(&canonical, &canonical_source);

            if entry.context_id.is_none()
                || entry.main_worktree_root.is_none()
                || entry.worktree_role.is_none()
                || entry.branch_status.is_none()
            {
                let context = registration_context(&canonical, &canonical_source);
                apply_context(entry, &context, &canonical, &canonical_source);
            }
        }
    }

    /// Collapse entries that share an [`EntryIdentity`], keeping the most
    /// recently registered survivor per identity while preserving the first-seen
    /// ordering of survivors. Pure and deterministic over the in-memory
    /// `projects` vec. Returns whether any duplicate rows were removed so the
    /// caller can decide whether to persist the compacted registry.
    ///
    /// This is the self-healing counterpart to identity-keyed upsert: even a
    /// registry already polluted with per-`head_oid` duplicates (issue #116)
    /// converges to one entry per `(project_root, source_root, branch)` on load.
    /// Like `upsert_project`, the survivor absorbs durable configuration
    /// (`indexing`) from collapsed duplicates so a newer lifecycle registration
    /// without indexing config never erases an older CLI registration's config.
    fn collapse_duplicate_entries(&mut self) -> bool {
        let before = self.projects.len();
        if before < 2 {
            return false;
        }

        let mut survivors: Vec<ProjectEntry> = Vec::with_capacity(before);
        let mut index_by_identity: HashMap<EntryIdentity, usize> = HashMap::new();

        for entry in std::mem::take(&mut self.projects) {
            let identity = entry_identity(&entry);
            match index_by_identity.get(&identity) {
                Some(&position) => {
                    if entry_supersedes(&entry, &survivors[position]) {
                        let mut entry = entry;
                        absorb_durable_config(&mut entry, &survivors[position]);
                        survivors[position] = entry;
                    } else {
                        absorb_durable_config(&mut survivors[position], &entry);
                    }
                }
                None => {
                    index_by_identity.insert(identity, survivors.len());
                    survivors.push(entry);
                }
            }
        }

        self.projects = survivors;
        self.projects.len() < before
    }
}

pub fn registration_context(project_root: &Path, source_root: &Path) -> WorktreeContext {
    let canonical = canonical_project_root(project_root);
    let canonical_source = canonical_project_root(source_root);

    if let Ok(resolved) = resolve_project_root(&canonical_source) {
        if canonical_project_root(&resolved.state_root) == canonical
            && resolved.worktree_context.worktree_role != WorktreeRole::Unknown
        {
            return resolved.worktree_context;
        }
    }

    legacy_worktree_context(&canonical, &canonical_source)
}

fn apply_context(
    entry: &mut ProjectEntry,
    context: &WorktreeContext,
    canonical: &Path,
    canonical_source: &Path,
) {
    entry.project_root = canonical.to_path_buf();
    entry.source_root = stored_source_root(canonical, canonical_source);
    entry.context_id = Some(context.context_id.clone());
    entry.main_worktree_root = Some(canonical_project_root(&context.main_worktree_root));
    entry.worktree_role = Some(context.worktree_role);
    entry.branch_name = context.branch_name.clone();
    entry.branch_ref = context.branch_ref.clone();
    entry.branch_status = Some(context.branch_status);
    entry.head_oid = context.head_oid.clone();
}

/// Pure identity of a registry entry. Two registrations collapse onto the same
/// entry exactly when their identities are equal. Identity is narrowed to the
/// stable `(project_root, source_root, branch)` triple and deliberately excludes
/// `head_oid`, `context_id`, `branch_status`, and `registered_at`, which are
/// mutable facts refreshed in place. Folding `head_oid` (and the `head_oid`-
/// derived `context_id`) into identity is what caused issue #116: every HEAD
/// advance on the same branch failed the match and appended a duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EntryIdentity {
    project_root: PathBuf,
    source_root: PathBuf,
    branch_ref: Option<String>,
}

fn entry_identity(entry: &ProjectEntry) -> EntryIdentity {
    EntryIdentity {
        project_root: canonical_project_root(&entry.project_root),
        source_root: canonical_project_root(entry.source_root()),
        branch_ref: entry.branch_ref.clone(),
    }
}

fn context_identity(
    canonical: &Path,
    canonical_source: &Path,
    context: &WorktreeContext,
) -> EntryIdentity {
    EntryIdentity {
        project_root: canonical.to_path_buf(),
        source_root: canonical_source.to_path_buf(),
        branch_ref: context.branch_ref.clone(),
    }
}

/// Whether `candidate` should replace `incumbent` as the survivor for a shared
/// identity: prefer the more recent `registered_at`, falling back to the more
/// complete entry when timestamps are equal or unparseable.
fn entry_supersedes(candidate: &ProjectEntry, incumbent: &ProjectEntry) -> bool {
    match (
        parse_registered_at(&candidate.registered_at),
        parse_registered_at(&incumbent.registered_at),
    ) {
        (Some(candidate_ts), Some(incumbent_ts)) if candidate_ts != incumbent_ts => {
            candidate_ts > incumbent_ts
        }
        _ => entry_completeness(candidate) > entry_completeness(incumbent),
    }
}

/// Merge durable, caller-provided configuration from a collapsed duplicate
/// into the surviving entry when the survivor lacks it. `indexing` is the only
/// such field: it is set exclusively by explicit registrations (the CLI), while
/// every other optional field on [`ProjectEntry`] is a mutable worktree fact
/// that `apply_context` refreshes wholesale, so the newer survivor's values
/// (including `None`) are authoritative for those. Mirrors `upsert_project`,
/// which keeps existing `indexing` when a registration passes `None`.
fn absorb_durable_config(survivor: &mut ProjectEntry, collapsed: &ProjectEntry) {
    if survivor.indexing.is_none() {
        survivor.indexing = collapsed.indexing.clone();
    }
}

fn parse_registered_at(value: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(value).ok()
}

fn entry_completeness(entry: &ProjectEntry) -> usize {
    [
        entry.source_root.is_some(),
        entry.context_id.is_some(),
        entry.main_worktree_root.is_some(),
        entry.worktree_role.is_some(),
        entry.branch_name.is_some(),
        entry.branch_ref.is_some(),
        entry.branch_status.is_some(),
        entry.head_oid.is_some(),
        entry.indexing.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count()
}

fn entry_matches_context(
    entry: &ProjectEntry,
    canonical: &Path,
    canonical_source: &Path,
    context: &WorktreeContext,
) -> bool {
    if entry.context_id.as_deref() == Some(context.context_id.as_str()) {
        return true;
    }

    entry_identity(entry) == context_identity(canonical, canonical_source, context)
}

fn stored_source_root(canonical: &Path, canonical_source: &Path) -> Option<PathBuf> {
    (canonical_source != canonical).then(|| canonical_source.to_path_buf())
}

fn legacy_worktree_context(project_root: &Path, source_root: &Path) -> WorktreeContext {
    WorktreeContext {
        context_id: legacy_context_id(project_root, source_root),
        state_root: project_root.to_path_buf(),
        source_root: source_root.to_path_buf(),
        main_worktree_root: project_root.to_path_buf(),
        worktree_role: if project_root == source_root {
            WorktreeRole::Main
        } else {
            WorktreeRole::Unknown
        },
        git_dir: None,
        common_git_dir: None,
        branch_name: None,
        branch_ref: None,
        head_oid: None,
        branch_status: BranchStatus::Unknown,
    }
}

fn legacy_context_id(project_root: &Path, source_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"oneup-worktree-context-v1\0");
    hasher.update(project_root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(source_root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(BranchStatus::Unknown.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(BranchStatus::Unknown.as_str().as_bytes());
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
struct RegistryLock {
    _lock: Flock<File>,
}

#[cfg(not(unix))]
struct RegistryLock;

#[cfg(unix)]
fn acquire_registry_lock(approved_root: &Path) -> Result<RegistryLock, OneupError> {
    let lock_path =
        validate_regular_file_path(&approved_root.join(REGISTRY_LOCK_FILE), approved_root)
            .map_err(|err| {
                DaemonError::WatcherError(format!("failed to validate registry lock path: {err}"))
            })?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(SECURE_STATE_FILE_MODE)
        .open(&lock_path)
        .map_err(|err| DaemonError::WatcherError(format!("failed to open registry lock: {err}")))?;
    fs::set_permissions(
        &lock_path,
        fs::Permissions::from_mode(SECURE_STATE_FILE_MODE),
    )
    .map_err(|err| DaemonError::WatcherError(format!("failed to secure registry lock: {err}")))?;

    Flock::lock(file, FlockArg::LockExclusive)
        .map(|lock| RegistryLock { _lock: lock })
        .map_err(|(_, errno)| {
            DaemonError::WatcherError(format!("failed to lock registry: {errno}")).into()
        })
}

#[cfg(not(unix))]
fn acquire_registry_lock(_approved_root: &Path) -> Result<RegistryLock, OneupError> {
    Ok(RegistryLock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[cfg(unix)]
    fn mode_bits(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn test_entry(
        project_id: &str,
        project_root: PathBuf,
        source_root: Option<PathBuf>,
        indexing: Option<IndexingConfig>,
    ) -> ProjectEntry {
        ProjectEntry {
            project_id: project_id.to_string(),
            project_root,
            source_root,
            context_id: None,
            main_worktree_root: None,
            worktree_role: None,
            branch_name: None,
            branch_ref: None,
            branch_status: None,
            head_oid: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing,
        }
    }

    fn test_context(
        project_root: &Path,
        source_root: &Path,
        context_id: &str,
        branch_name: &str,
    ) -> WorktreeContext {
        WorktreeContext {
            context_id: context_id.to_string(),
            state_root: project_root.to_path_buf(),
            source_root: source_root.to_path_buf(),
            main_worktree_root: project_root.to_path_buf(),
            worktree_role: if project_root == source_root {
                WorktreeRole::Main
            } else {
                WorktreeRole::Linked
            },
            git_dir: None,
            common_git_dir: None,
            branch_name: Some(branch_name.to_string()),
            branch_ref: Some(format!("refs/heads/{branch_name}")),
            head_oid: Some(format!("{:0>40}", context_id)),
            branch_status: BranchStatus::Named,
        }
    }

    #[test]
    fn registry_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("projects.json");

        let project_dir = tmp.path().join("myproject");
        fs::create_dir_all(&project_dir).unwrap();

        let mut reg = Registry::default();
        reg.projects.push(test_entry(
            "abc-123",
            project_dir.clone(),
            None,
            Some(IndexingConfig::new(4, 2, 1).unwrap()),
        ));

        let content = serde_json::to_string_pretty(&reg).unwrap();
        fs::write(&registry_path, &content).unwrap();

        let loaded: Registry =
            serde_json::from_str(&fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "abc-123");
        assert_eq!(
            loaded.projects[0].indexing,
            Some(IndexingConfig::new(4, 2, 1).unwrap())
        );
    }

    #[test]
    fn deregister_removes_project() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let mut reg = Registry::default();
        reg.projects.push(test_entry(
            "id-a",
            dir_a.canonicalize().unwrap(),
            None,
            None,
        ));
        reg.projects.push(test_entry(
            "id-b",
            dir_b.canonicalize().unwrap(),
            None,
            None,
        ));

        let before = reg.projects.len();
        let canonical_a = dir_a.canonicalize().unwrap();
        reg.projects.retain(|p| p.project_root != canonical_a);
        assert_eq!(reg.projects.len(), before - 1);
        assert_eq!(reg.projects[0].project_id, "id-b");
    }

    #[test]
    fn empty_registry() {
        let reg = Registry::default();
        assert!(reg.is_empty());
        assert!(reg.project_roots().is_empty());
    }

    #[test]
    fn registry_deserializes_older_entries_without_indexing() {
        let raw = r#"
        {
          "projects": [
            {
              "project_id": "abc-123",
              "project_root": "/tmp/project",
              "registered_at": "2026-01-01T00:00:00Z"
            }
          ]
        }
        "#;

        let loaded: Registry = serde_json::from_str(raw).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert!(loaded.projects[0].indexing.is_none());
    }

    #[test]
    fn registry_loads_older_entries_as_main_worktree_contexts() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("project");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();

        fs::write(
            &registry_path,
            format!(
                r#"{{
                  "projects": [
                    {{
                      "project_id": "abc-123",
                      "project_root": "{}",
                      "registered_at": "2026-01-01T00:00:00Z"
                    }}
                  ]
                }}"#,
                project.display()
            ),
        )
        .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        let entry = &loaded.projects[0];
        assert_eq!(entry.project_root, project);
        assert_eq!(entry.source_root, None);
        assert_eq!(entry.main_worktree_root(), entry.project_root.as_path());
        assert_eq!(entry.worktree_role(), WorktreeRole::Main);
        assert_eq!(entry.branch_status(), BranchStatus::Unknown);
        assert_eq!(entry.context_id().len(), 32);
    }

    #[test]
    fn registry_defaults_missing_indexing_fields() {
        let raw = r#"
        {
          "projects": [
            {
              "project_id": "abc-123",
              "project_root": "/tmp/project",
              "registered_at": "2026-01-01T00:00:00Z",
              "indexing": {
                "jobs": 2
              }
            }
          ]
        }
        "#;

        let loaded: Registry = serde_json::from_str(raw).unwrap();
        let indexing = loaded.projects[0].indexing.as_ref().unwrap();
        assert_eq!(indexing.jobs, 2);
        assert_eq!(indexing.embed_threads, 2);
        assert_eq!(
            indexing.write_batch_files,
            crate::shared::types::IndexingConfig::default_write_batch_files_for(indexing.jobs)
        );
    }

    #[test]
    fn registry_save_secures_xdg_root_and_registry_file() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().canonicalize().unwrap().join("xdg-root");
        let registry_path = xdg_root.join("projects.json");

        fs::create_dir_all(&xdg_root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&xdg_root, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let project_root = tmp.path().join("project");
        fs::create_dir_all(&project_root).unwrap();

        let mut registry = Registry::default();
        registry.projects.push(test_entry(
            "abc-123",
            project_root,
            None,
            Some(IndexingConfig::new(4, 2, 1).unwrap()),
        ));

        registry.save_to_path(&registry_path, &xdg_root).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(mode_bits(&xdg_root), XDG_STATE_DIR_MODE);
            assert_eq!(mode_bits(&registry_path), SECURE_STATE_FILE_MODE);
        }
        assert_eq!(
            Registry::load_from_path(&registry_path, &xdg_root)
                .unwrap()
                .projects
                .len(),
            1
        );
    }

    #[test]
    fn registry_register_reloads_locked_state_before_saving() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project_a = tmp_root.join("project-a");
        let project_b = tmp_root.join("project-b");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();

        let mut first = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        let mut stale_second = first.clone();

        first
            .register_at_path(
                "id-a",
                &project_a,
                &project_a,
                None,
                &registry_path,
                &xdg_root,
            )
            .unwrap();
        stale_second
            .register_at_path(
                "id-b",
                &project_b,
                &project_b,
                None,
                &registry_path,
                &xdg_root,
            )
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 2);
        assert!(loaded
            .projects
            .iter()
            .any(|entry| entry.project_id == "id-a"));
        assert!(loaded
            .projects
            .iter()
            .any(|entry| entry.project_id == "id-b"));
        assert_eq!(stale_second.projects.len(), 2);
    }

    #[test]
    fn registry_register_keeps_one_entry_for_same_project_under_stale_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("project");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();

        let mut first = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        let mut stale_second = first.clone();

        first
            .register_at_path("id-a", &project, &project, None, &registry_path, &xdg_root)
            .unwrap();
        stale_second
            .register_at_path("id-b", &project, &project, None, &registry_path, &xdg_root)
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "id-b");
        assert_eq!(stale_second.projects.len(), 1);
    }

    #[test]
    fn registry_register_persists_distinct_source_root() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        let source = tmp_root.join("worktree");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&source).unwrap();

        let mut registry = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        registry
            .register_at_path("id", &project, &source, None, &registry_path, &xdg_root)
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_root, project);
        assert_eq!(loaded.projects[0].source_root, Some(source));
    }

    #[test]
    fn registry_register_preserves_multiple_source_roots_for_same_project() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        let linked = tmp_root.join("linked");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&linked).unwrap();

        let mut registry = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        registry
            .register_at_path(
                "main-id",
                &project,
                &project,
                None,
                &registry_path,
                &xdg_root,
            )
            .unwrap();
        registry
            .register_at_path(
                "linked-id",
                &project,
                &linked,
                None,
                &registry_path,
                &xdg_root,
            )
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 2);
        assert!(loaded
            .projects
            .iter()
            .any(|entry| entry.source_root.is_none()));
        assert!(loaded
            .projects
            .iter()
            .any(|entry| entry.source_root.as_deref() == Some(linked.as_path())));
    }

    #[test]
    fn registry_register_preserves_multiple_branch_contexts_for_same_source_root() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();

        let context_a = test_context(
            &project,
            &project,
            "11111111111111111111111111111111",
            "main",
        );
        let context_b = test_context(
            &project,
            &project,
            "22222222222222222222222222222222",
            "feature",
        );

        let mut registry = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        registry
            .register_context_at_path("main-id", &context_a, None, &registry_path, &xdg_root)
            .unwrap();
        registry
            .register_context_at_path("feature-id", &context_b, None, &registry_path, &xdg_root)
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 2);
        assert!(loaded.projects.iter().any(|entry| {
            entry.branch_ref.as_deref() == Some("refs/heads/main") && entry.project_id == "main-id"
        }));
        assert!(loaded.projects.iter().any(|entry| {
            entry.branch_ref.as_deref() == Some("refs/heads/feature")
                && entry.project_id == "feature-id"
        }));
    }

    #[test]
    fn registry_deregister_context_removes_only_matching_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        let linked = tmp_root.join("linked");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&linked).unwrap();

        let main_context = test_context(
            &project,
            &project,
            "11111111111111111111111111111111",
            "main",
        );
        let linked_context = test_context(
            &project,
            &linked,
            "22222222222222222222222222222222",
            "feature",
        );

        let mut registry = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        registry
            .register_context_at_path("main-id", &main_context, None, &registry_path, &xdg_root)
            .unwrap();
        registry
            .register_context_at_path(
                "linked-id",
                &linked_context,
                None,
                &registry_path,
                &xdg_root,
            )
            .unwrap();

        let removed = registry
            .deregister_context_at_path(&linked_context, &registry_path, &xdg_root)
            .unwrap();

        assert!(removed);
        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "main-id");
        assert!(loaded.contains_context(&main_context));
        assert!(!loaded.contains_context(&linked_context));
    }

    #[test]
    fn registry_deregister_context_falls_back_only_for_single_project_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().canonicalize().unwrap().join("main");
        let source = project.join("source");
        fs::create_dir_all(&source).unwrap();

        let mut single = Registry::default();
        single.projects.push(test_entry(
            "legacy-id",
            project.clone(),
            Some(source.clone()),
            None,
        ));
        single.normalize_entries();

        let root_context = test_context(
            &project,
            &project,
            "11111111111111111111111111111111",
            "main",
        );
        assert!(single.remove_context(&root_context));
        assert!(single.projects.is_empty());

        let linked_context = registration_context(&project, &source);
        let mut multiple = Registry::default();
        multiple
            .projects
            .push(test_entry("main-id", project.clone(), None, None));
        multiple
            .projects
            .push(test_entry("linked-id", project.clone(), Some(source), None));
        multiple.normalize_entries();

        assert!(!multiple.remove_context(&root_context));
        assert_eq!(multiple.projects.len(), 2);
        assert!(multiple.remove_context(&linked_context));
        assert_eq!(multiple.projects.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn registry_load_rejects_symlinked_registry_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let outside_root = tmp_root.join("outside");
        let registry_path = xdg_root.join("projects.json");

        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&outside_root).unwrap();
        fs::write(outside_root.join("projects.json"), "{}").unwrap();
        symlink(outside_root.join("projects.json"), &registry_path).unwrap();

        let err = Registry::load_from_path(&registry_path, &xdg_root).unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[test]
    fn registry_register_is_idempotent_across_head_advance() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();

        let before_advance = test_context(
            &project,
            &project,
            "11111111111111111111111111111111",
            "main",
        );
        let after_advance = test_context(
            &project,
            &project,
            "22222222222222222222222222222222",
            "main",
        );

        let mut registry = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        registry
            .register_context_at_path("id-1", &before_advance, None, &registry_path, &xdg_root)
            .unwrap();
        registry
            .register_context_at_path("id-2", &after_advance, None, &registry_path, &xdg_root)
            .unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        let entry = &loaded.projects[0];
        assert_eq!(entry.project_id, "id-2");
        assert_eq!(entry.head_oid, after_advance.head_oid);
        assert_eq!(
            entry.context_id.as_deref(),
            Some(after_advance.context_id.as_str())
        );
        assert_eq!(entry.branch_ref.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn collapse_duplicate_entries_keeps_newest_per_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let project_a = tmp_root.join("a");
        let project_b = tmp_root.join("b");
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();

        let mut older = test_entry("a-old", project_a.clone(), None, None);
        older.registered_at = "2026-01-01T00:00:00Z".to_string();
        let mut newer = test_entry("a-new", project_a.clone(), None, None);
        newer.registered_at = "2026-06-01T00:00:00Z".to_string();
        let mut newest = test_entry("a-newest", project_a.clone(), None, None);
        newest.registered_at = "2026-09-01T00:00:00Z".to_string();
        let untouched = test_entry("b-only", project_b.clone(), None, None);

        let mut registry = Registry {
            projects: vec![older, newest, newer, untouched],
        };
        registry.normalize_entries();

        let collapsed = registry.collapse_duplicate_entries();

        assert!(collapsed);
        assert_eq!(registry.projects.len(), 2);
        let a_entry = registry
            .projects
            .iter()
            .find(|entry| entry.project_root == project_a)
            .unwrap();
        assert_eq!(a_entry.project_id, "a-newest");
        assert!(registry
            .projects
            .iter()
            .any(|entry| entry.project_id == "b-only"));
    }

    #[test]
    fn collapse_duplicate_entries_is_noop_without_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let project_a = tmp_root.join("a");
        let project_b = tmp_root.join("b");
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();

        let mut registry = Registry {
            projects: vec![
                test_entry("a", project_a, None, None),
                test_entry("b", project_b, None, None),
            ],
        };
        registry.normalize_entries();

        assert!(!registry.collapse_duplicate_entries());
        assert_eq!(registry.projects.len(), 2);
    }

    #[test]
    fn registry_load_deduplicates_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();

        let mut older = test_entry(
            "old",
            project.clone(),
            None,
            Some(IndexingConfig::new(4, 2, 1).unwrap()),
        );
        older.registered_at = "2026-01-01T00:00:00Z".to_string();
        let mut newer = test_entry("new", project.clone(), None, None);
        newer.registered_at = "2026-08-01T00:00:00Z".to_string();
        let polluted = Registry {
            projects: vec![older, newer],
        };
        fs::write(
            &registry_path,
            serde_json::to_string_pretty(&polluted).unwrap(),
        )
        .unwrap();

        let loaded = Registry::load_from_path_with_repair(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "new");
        // Durable config from the collapsed older CLI registration survives.
        assert_eq!(
            loaded.projects[0].indexing,
            Some(IndexingConfig::new(4, 2, 1).unwrap())
        );

        // The repair persisted the compacted registry: the raw file holds one row.
        let raw: Registry =
            serde_json::from_str(&fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(raw.projects.len(), 1);
        assert_eq!(raw.projects[0].project_id, "new");
        assert_eq!(
            raw.projects[0].indexing,
            Some(IndexingConfig::new(4, 2, 1).unwrap())
        );
    }

    #[test]
    fn registry_plain_load_never_writes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();

        let mut older = test_entry("old", project.clone(), None, None);
        older.registered_at = "2026-01-01T00:00:00Z".to_string();
        let mut newer = test_entry("new", project.clone(), None, None);
        newer.registered_at = "2026-08-01T00:00:00Z".to_string();
        let polluted = Registry {
            projects: vec![older, newer],
        };
        let raw_before = serde_json::to_string_pretty(&polluted).unwrap();
        fs::write(&registry_path, &raw_before).unwrap();

        let loaded = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "new");

        // The unlocked plain load deduplicated only in memory.
        assert_eq!(fs::read_to_string(&registry_path).unwrap(), raw_before);
    }

    #[test]
    fn registry_repair_rereads_fresh_state_and_preserves_concurrent_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project_a = tmp_root.join("project-a");
        let project_b = tmp_root.join("project-b");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();

        // 1. On-disk registry is polluted with duplicates for project A.
        let mut older = test_entry("a-old", project_a.clone(), None, None);
        older.registered_at = "2026-01-01T00:00:00Z".to_string();
        let mut newer = test_entry("a-new", project_a.clone(), None, None);
        newer.registered_at = "2026-08-01T00:00:00Z".to_string();
        let polluted = Registry {
            projects: vec![older, newer],
        };
        fs::write(
            &registry_path,
            serde_json::to_string_pretty(&polluted).unwrap(),
        )
        .unwrap();

        // 2. A reader takes an unlocked snapshot and sees duplicates, so it
        //    will attempt a repair. The snapshot itself must not write.
        let stale_snapshot = Registry::load_from_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(stale_snapshot.projects.len(), 1);

        // 3. Before the repair runs, a concurrent registration saves project B.
        let mut concurrent = Registry::default();
        concurrent
            .register_at_path(
                "b-id",
                &project_b,
                &project_b,
                None,
                &registry_path,
                &xdg_root,
            )
            .unwrap();

        // 4. The reader's repair must lock and re-read fresh state rather than
        //    persist its stale snapshot, so B's registration survives.
        let repaired = Registry::repair_duplicates_at_path(&registry_path, &xdg_root).unwrap();
        assert_eq!(repaired.projects.len(), 2);

        let on_disk: Registry =
            serde_json::from_str(&fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(on_disk.projects.len(), 2);
        assert!(on_disk
            .projects
            .iter()
            .any(|entry| entry.project_id == "a-new"));
        assert!(on_disk
            .projects
            .iter()
            .any(|entry| entry.project_id == "b-id"));
    }

    #[cfg(unix)]
    #[test]
    fn registry_load_with_repair_swallows_repair_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let xdg_root = tmp_root.join("xdg-root");
        let registry_path = xdg_root.join("projects.json");
        let project = tmp_root.join("main");
        fs::create_dir_all(&xdg_root).unwrap();
        fs::create_dir_all(&project).unwrap();

        let mut older = test_entry("old", project.clone(), None, None);
        older.registered_at = "2026-01-01T00:00:00Z".to_string();
        let mut newer = test_entry("new", project.clone(), None, None);
        newer.registered_at = "2026-08-01T00:00:00Z".to_string();
        let polluted = Registry {
            projects: vec![older, newer],
        };
        let raw_before = serde_json::to_string_pretty(&polluted).unwrap();
        fs::write(&registry_path, &raw_before).unwrap();

        // A directory at the lock path makes lock acquisition fail, so the
        // repair errs. Load must swallow that and still return the in-memory
        // dedup while leaving the duplicated file untouched.
        fs::create_dir_all(xdg_root.join(REGISTRY_LOCK_FILE)).unwrap();

        let loaded = Registry::load_from_path_with_repair(&registry_path, &xdg_root).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project_id, "new");
        assert_eq!(fs::read_to_string(&registry_path).unwrap(), raw_before);
    }

    #[test]
    fn collapse_duplicate_entries_preserves_indexing_from_collapsed_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let project = tmp_root.join("main");
        fs::create_dir_all(&project).unwrap();

        let indexing = IndexingConfig::new(4, 2, 1).unwrap();
        // Fully-populated duplicates (same identity, distinct mutable facts) so
        // `normalize_entries` leaves them untouched, matching a real polluted
        // registry written by successive registrations.
        let make_duplicate = |project_id: &str, registered_at: &str, head_byte: &str| {
            let mut entry = test_entry(project_id, project.clone(), None, None);
            entry.context_id = Some(head_byte.repeat(32));
            entry.main_worktree_root = Some(project.clone());
            entry.worktree_role = Some(WorktreeRole::Main);
            entry.branch_name = Some("main".to_string());
            entry.branch_ref = Some("refs/heads/main".to_string());
            entry.branch_status = Some(BranchStatus::Named);
            entry.head_oid = Some(head_byte.repeat(40));
            entry.registered_at = registered_at.to_string();
            entry
        };
        let make_cli_entry = || {
            let mut entry = make_duplicate("cli-id", "2026-01-01T00:00:00Z", "a");
            entry.indexing = Some(indexing.clone());
            entry
        };
        let make_lifecycle_entry = || make_duplicate("lifecycle-id", "2026-08-01T00:00:00Z", "b");

        // Both file orders: older-first (newer entry supersedes the incumbent)
        // and newer-first (older entry loses against the incumbent).
        for projects in [
            vec![make_cli_entry(), make_lifecycle_entry()],
            vec![make_lifecycle_entry(), make_cli_entry()],
        ] {
            let mut registry = Registry { projects };
            registry.normalize_entries();

            assert!(registry.collapse_duplicate_entries());
            assert_eq!(registry.projects.len(), 1);
            let survivor = &registry.projects[0];
            // Newer facts win...
            assert_eq!(survivor.project_id, "lifecycle-id");
            assert_eq!(survivor.registered_at, "2026-08-01T00:00:00Z");
            assert_eq!(survivor.head_oid.as_deref(), Some("b".repeat(40).as_str()));
            // ...but the durable indexing config is not erased.
            assert_eq!(survivor.indexing, Some(indexing.clone()));
        }
    }

    #[test]
    fn collapse_duplicate_entries_keeps_survivor_indexing_over_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let project = tmp_root.join("main");
        fs::create_dir_all(&project).unwrap();

        let mut older = test_entry(
            "old",
            project.clone(),
            None,
            Some(IndexingConfig::new(2, 1, 1).unwrap()),
        );
        older.registered_at = "2026-01-01T00:00:00Z".to_string();
        let mut newer = test_entry(
            "new",
            project.clone(),
            None,
            Some(IndexingConfig::new(8, 4, 2).unwrap()),
        );
        newer.registered_at = "2026-08-01T00:00:00Z".to_string();

        let mut registry = Registry {
            projects: vec![older, newer],
        };
        registry.normalize_entries();

        assert!(registry.collapse_duplicate_entries());
        assert_eq!(registry.projects.len(), 1);
        assert_eq!(registry.projects[0].project_id, "new");
        assert_eq!(
            registry.projects[0].indexing,
            Some(IndexingConfig::new(8, 4, 2).unwrap())
        );
    }
}
